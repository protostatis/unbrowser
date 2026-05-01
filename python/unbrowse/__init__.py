"""unbrowse — Python client for the unbrowse binary.

`pip install unbrowse` ships the native binary inside the wheel for your
platform — there's nothing else to install. Use it like:

    from unbrowse import Client

    with Client() as ub:
        r = ub.navigate("https://news.ycombinator.com")
        for s in ub.query(".titleline > a")[:3]:
            print(s["text"], s["attrs"]["href"])

For the `extract` / auto-strategy command, watchdog-bounded `exec_scripts`,
the cookie handoff for bot-walled sites, and the BlockMap shape: see the
project README at https://github.com/protostatis/unbrowse.
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

__version__ = "0.0.1"

__all__ = ["Client", "UnbrowseError", "find_binary", "navigate", "__version__"]


class UnbrowseError(Exception):
    """Raised when the binary returns a JSON-RPC error or can't be spawned."""


def find_binary() -> str:
    """Resolve the unbrowse binary path.

    Resolution order, most-explicit first:

      1. ``UNBROWSE_BIN`` env var (overrides everything; right escape hatch
         for testing a one-off build or vendored copy).
      2. Bundled binary inside this package (the wheel ships one for your
         platform — this is what end users hit).
      3. ``unbrowse`` on ``$PATH`` (covers ``cargo install`` / ``brew install``
         users who didn't install the wheel).
      4. The local debug build at ``target/debug/unbrowse`` relative to the
         repo root (developer convenience — only fires when running from a
         checkout without an installed wheel).

    Raises UnbrowseError with a helpful message if none of the above resolve.
    """
    env = os.environ.get("UNBROWSE_BIN")
    if env:
        if not Path(env).is_file():
            raise UnbrowseError(
                f"UNBROWSE_BIN points to {env!r}, which doesn't exist"
            )
        return env

    bundled = Path(__file__).parent / "_bin" / _binary_name()
    if bundled.is_file():
        return str(bundled)

    on_path = shutil.which("unbrowse")
    if on_path:
        return on_path

    # Dev fallback: target/debug/unbrowse two dirs up from this file
    # (python/unbrowse/__init__.py -> python/unbrowse -> python -> repo root).
    dev = Path(__file__).resolve().parents[2] / "target" / "debug" / "unbrowse"
    if dev.is_file():
        return str(dev)

    raise UnbrowseError(
        "Could not locate the unbrowse binary. Tried: $UNBROWSE_BIN, "
        "package-bundled binary, $PATH, target/debug/unbrowse. "
        "Install via `pip install unbrowse` (ships the binary), "
        "`cargo install unbrowse`, or `brew install unbrowse`."
    )


def _binary_name() -> str:
    return "unbrowse.exe" if sys.platform == "win32" else "unbrowse"


class Client:
    """Synchronous JSON-RPC client for the unbrowse binary.

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
        # Belt-and-braces orphan prevention: if the interpreter exits before
        # __exit__ runs (unhandled exception, sys.exit, heredoc-wrapped
        # invocation killed mid-flight), atexit reaps the subprocess. The
        # binary's own watchdog bounds JS execution; this covers the
        # subprocess-lifecycle layer.
        atexit.register(self._reap)

    # ---- core RPC --------------------------------------------------------

    def call(self, method: str, **params) -> Any:
        """Send one JSON-RPC request, return the result. Raises UnbrowseError on RPC error."""
        self._next_id += 1
        req = {"id": self._next_id, "method": method, "params": params}
        assert self._proc.stdin is not None and self._proc.stdout is not None
        self._proc.stdin.write(json.dumps(req) + "\n")
        self._proc.stdin.flush()
        line = self._proc.stdout.readline()
        if not line:
            raise UnbrowseError(f"binary closed stdout while waiting for {method}")
        resp = json.loads(line)
        if "error" in resp:
            raise UnbrowseError(f"{method}: {resp['error']}")
        return resp.get("result")

    # ---- typed wrappers (don't add behavior; just discoverability) -------

    def navigate(self, url: str, exec_scripts: bool = False) -> dict:
        return self.call("navigate", url=url, exec_scripts=exec_scripts)

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
        return self.call("click", ref=ref)

    def type(self, ref: str, text: str) -> dict:
        return self.call("type", ref=ref, text=text)

    def submit(self, ref: str) -> dict:
        return self.call("submit", ref=ref)

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
        except (UnbrowseError, BrokenPipeError, OSError):
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
