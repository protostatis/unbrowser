"""unbrowser — Python client for the unbrowser binary.

`pip install unbrowser` ships the native binary inside the wheel for your
platform — there's nothing else to install. Use it like:

    from unbrowser import Client

    with Client() as ub:
        r = ub.navigate("https://news.ycombinator.com")
        for s in ub.query(".titleline > a")[:3]:
            print(s["text"], s["attrs"]["href"])

For the `extract` / auto-strategy command, watchdog-bounded `exec_scripts`,
the cookie handoff for bot-walled sites, and the BlockMap shape: see the
project README at https://github.com/protostatis/unbrowser.
"""

from __future__ import annotations

import atexit
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any
from urllib.parse import quote_plus, urljoin, urlparse

__version__ = "0.0.1"

__all__ = ["Client", "UnbrowserError", "find_binary", "navigate", "__version__"]


class UnbrowserError(Exception):
    """Raised when the binary returns a JSON-RPC error or can't be spawned."""


def find_binary() -> str:
    """Resolve the unbrowser binary path.

    Resolution order, most-explicit first:

      1. ``UNBROWSER_BIN`` env var (overrides everything; right escape hatch
         for testing a one-off build or vendored copy).
      2. Bundled binary inside this package (the wheel ships one for your
         platform — this is what end users hit).
      3. ``unbrowser`` on ``$PATH`` (covers ``cargo install`` / ``brew install``
         users who didn't install the wheel).
      4. The local debug build at ``target/debug/unbrowser`` relative to the
         repo root (developer convenience — only fires when running from a
         checkout without an installed wheel).

    Raises UnbrowserError with a helpful message if none of the above resolve.
    """
    env = os.environ.get("UNBROWSER_BIN")
    if env:
        if not Path(env).is_file():
            raise UnbrowserError(
                f"UNBROWSER_BIN points to {env!r}, which doesn't exist"
            )
        return env

    bundled = Path(__file__).parent / "_bin" / _binary_name()
    if bundled.is_file():
        return str(bundled)

    on_path = shutil.which("unbrowser")
    if on_path:
        return on_path

    # Dev fallback: target/debug/unbrowser two dirs up from this file
    # (python/unbrowser/__init__.py -> python/unbrowser -> python -> repo root).
    dev = Path(__file__).resolve().parents[2] / "target" / "debug" / "unbrowser"
    if dev.is_file():
        return str(dev)

    raise UnbrowserError(
        "Could not locate the unbrowser binary. Tried: $UNBROWSER_BIN, "
        "package-bundled binary, $PATH, target/debug/unbrowser. "
        "Install via `pip install unbrowser` (ships the binary), "
        "`cargo install unbrowser`, or `brew install unbrowser`."
    )


def _binary_name() -> str:
    return "unbrowser.exe" if sys.platform == "win32" else "unbrowser"


class Client:
    """Synchronous JSON-RPC client for the unbrowser binary.

    One subprocess per Client. The session (cookies, last_url, last_body)
    persists across calls until close().
    """

    def __init__(self, binary: str | None = None):
        self._proc = subprocess.Popen(
            [binary or find_binary()],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        self._next_id = 0
        self._closed = False
        # Track the last successful navigate URL Python-side so make_absolute_url
        # can resolve relative hrefs without round-tripping to the binary. Updated
        # after every successful navigate / submit / click-with-follow.
        self._last_url: str | None = None
        # Belt-and-braces orphan prevention: if the interpreter exits before
        # __exit__ runs (unhandled exception, sys.exit, heredoc-wrapped
        # invocation killed mid-flight), atexit reaps the subprocess. The
        # binary's own watchdog bounds JS execution; this covers the
        # subprocess-lifecycle layer.
        atexit.register(self._reap)

    # ---- core RPC --------------------------------------------------------

    def call(self, method: str, **params) -> Any:
        """Send one JSON-RPC request, return the result. Raises UnbrowserError on RPC error."""
        self._next_id += 1
        req = {"id": self._next_id, "method": method, "params": params}
        assert self._proc.stdin is not None and self._proc.stdout is not None
        self._proc.stdin.write(json.dumps(req) + "\n")
        self._proc.stdin.flush()
        line = self._proc.stdout.readline()
        if not line:
            raise UnbrowserError(f"binary closed stdout while waiting for {method}")
        resp = json.loads(line)
        if "error" in resp:
            raise UnbrowserError(f"{method}: {resp['error']}")
        return resp.get("result")

    # ---- typed wrappers (don't add behavior; just discoverability) -------

    def navigate(self, url: str, exec_scripts: bool = False) -> dict:
        r = self.call("navigate", url=url, exec_scripts=exec_scripts)
        if isinstance(r, dict) and r.get("url"):
            self._last_url = r["url"]
        return r

    def query(self, selector: str) -> list[dict]:
        return self.call("query", selector=selector)

    def text(self, selector: str = "body") -> str | None:
        return self.call("text", selector=selector)

    def text_main(self) -> str | None:
        """textContent of the main content area (excludes header/nav/footer/aside)."""
        return self.call("text_main")

    def query_text(self, text: str, selector: str | None = None,
                   exact: bool = False, limit: int = 20) -> list[dict]:
        """Find elements by visible text content (chrome-stripped, deepest match).

        Use when CSS selectors are unstable (React-rendered pages) but the
        visible label is reliable, e.g. r.query_text('Sign in')[0].
        """
        params: dict = {"text": text, "exact": exact, "limit": limit}
        if selector is not None:
            params["selector"] = selector
        return self.call("query_text", **params)

    def click(self, ref: str) -> dict:
        r = self.call("click", ref=ref)
        # click on <a href> auto-follows and returns navigate-shape result;
        # update last_url so make_absolute_url stays accurate.
        if isinstance(r, dict) and r.get("url"):
            self._last_url = r["url"]
        return r

    def type(self, ref: str, text: str) -> dict:
        return self.call("type", ref=ref, text=text)

    def submit(self, ref: str) -> dict:
        r = self.call("submit", ref=ref)
        if isinstance(r, dict) and r.get("url"):
            self._last_url = r["url"]
        return r

    def search(self, query: str, engine: str = "ddg") -> dict:
        """Search via the named engine; return the navigate result.

        Engines:
            ddg   — DuckDuckGo HTML (default; reliable, returns SSR'd results
                    that the cheap path can extract directly via query()).
            bing  — Bing search (also works without exec_scripts).

        Google is intentionally NOT supported via the cheap path — Google's
        search page returns ~no useful HTML without JS, so it would silently
        fail. Use the cookie-handoff escalation path or one of the supported
        engines instead.

        Use after navigate(), or as the first call: it kicks off its own
        navigate. The returned dict is the same shape as Client.navigate.
        """
        if engine == "ddg":
            url = "https://duckduckgo.com/html/?q=" + quote_plus(query)
        elif engine == "bing":
            url = "https://www.bing.com/search?q=" + quote_plus(query)
        else:
            raise UnbrowserError(
                f"unknown search engine '{engine}'. Supported: ddg, bing. "
                "Google is intentionally unsupported via the cheap path."
            )
        return self.navigate(url)

    def make_absolute_url(self, href: str) -> str:
        """Resolve a relative href against the current page URL.

        Use after navigate to expand `<a href="/foo">` or `href="../bar"`
        into a full URL. If `href` is already absolute (has scheme + host),
        it's returned unchanged — preventing the double-prefix class of bug
        you get from naive ``current_url + href`` concatenation.

        Raises UnbrowserError if no page has been navigated yet.
        """
        if not href:
            raise UnbrowserError("empty href")
        parsed = urlparse(href)
        if parsed.scheme and parsed.netloc:
            return href
        if not self._last_url:
            raise UnbrowserError("no current page — call navigate first")
        return urljoin(self._last_url, href)

    def blockmap(self) -> dict:
        return self.call("blockmap")

    def extract(self, strategy: str | None = None) -> dict:
        """Auto-strategy structured-data extraction.

        Tries JSON-LD → __NEXT_DATA__ → Nuxt → OpenGraph/meta → microdata →
        text_main fallback, returns the highest-confidence hit as
        {strategy, confidence, data, tried}. Pass strategy='json_ld' (etc.)
        to force a specific extractor.
        """
        if strategy is None:
            return self.call("extract")
        return self.call("extract", strategy=strategy)

    def settle(self, max_ms: int = 2000, max_iters: int = 50) -> dict:
        """Drain the JS event loop: microtasks + setTimeout/setInterval.

        Returns when queue empty, max_ms elapses, or max_iters hit. Result:
        {iters, elapsed_ms, microtasks_run, timers_fired, pending_timers,
         pending_microtasks, timed_out}.
        """
        return self.call("settle", max_ms=max_ms, max_iters=max_iters)

    def body(self) -> str:
        return self.call("body")

    def eval(self, code: str) -> Any:
        """Run arbitrary JS in the session.

        Returns the JS expression's result already JSON-decoded into a Python
        value (dict / list / str / int / float / bool / None). DO NOT call
        json.loads() on the return value — the wrapper already did. The Rust
        side runs JSON.stringify on the JS value, the JSON-RPC framing parses
        it once, and the result is the Python equivalent.

        Errors surface the real JS exception name + message
        (`TypeError: cannot read property 'foo' of null` etc.) so iteration
        is fast.
        """
        return self.call("eval", code=code)

    def cookies_set(self, cookies: list[dict], url: str | None = None) -> dict:
        if url is None:
            return self.call("cookies_set", cookies=cookies)
        return self.call("cookies_set", cookies=cookies, url=url)

    def cookies_get(self) -> list[dict]:
        return self.call("cookies_get")

    def cookies_clear(self) -> dict:
        return self.call("cookies_clear")

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            self.call("close")
        except (UnbrowserError, BrokenPipeError, OSError):
            pass
        self._reap()

    def _reap(self) -> None:
        # Idempotent: stdin EOF first (binary's reader returns None and the
        # RPC loop exits cleanly), then escalate via terminate → kill if it
        # doesn't respond. Always wait() at the end so we don't leave a
        # zombie. Called from both close() and atexit.
        if self._proc.poll() is not None:
            return
        try:
            if self._proc.stdin and not self._proc.stdin.closed:
                self._proc.stdin.close()
        except (BrokenPipeError, OSError):
            pass
        try:
            self._proc.wait(timeout=2)
            return
        except subprocess.TimeoutExpired:
            pass
        self._proc.terminate()
        try:
            self._proc.wait(timeout=2)
            return
        except subprocess.TimeoutExpired:
            pass
        self._proc.kill()
        try:
            self._proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass

    # ---- context manager -------------------------------------------------

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *exc) -> None:
        self.close()


def navigate(url: str) -> dict:
    """One-shot: fetch a URL and return the navigate result. Closes immediately."""
    with Client() as ub:
        return ub.navigate(url)
