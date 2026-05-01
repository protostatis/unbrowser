"""unbrowse.py — minimal Python client for the unbrowse binary.

Usage:

    from unbrowse import Client

    with Client() as ub:
        r = ub.navigate("https://news.ycombinator.com")
        for s in ub.query(".titleline > a")[:3]:
            print(s["text"], s["attrs"]["href"])

That's it. No subprocess boilerplate to rewrite.

For auto-escalation on bot-walled sites (cookie handoff to real Chrome),
use `Router` from router.py instead — same shape but with a pluggable
chrome_solver callback.
"""

from __future__ import annotations

import json
import os
import subprocess
from typing import Any


_DEFAULT_BIN = os.environ.get(
    "UNBROWSE_BIN",
    "/Users/zhiminzou/Projects/unchained_browser/target/debug/unbrowse",
)


class UnbrowseError(Exception):
    pass


class Client:
    """Synchronous JSON-RPC client for the unbrowse binary.

    One subprocess per Client. The session (cookies, last_url, last_body)
    persists across calls until close().
    """

    def __init__(self, binary: str = _DEFAULT_BIN):
        self._proc = subprocess.Popen(
            [binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        self._next_id = 0

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
        try:
            self.call("close")
        except UnbrowseError:
            pass
        try:
            self._proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self._proc.kill()

    # ---- context manager -------------------------------------------------

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *exc) -> None:
        self.close()


# ---- one-liner shortcut for trivial cases --------------------------------

def navigate(url: str) -> dict:
    """One-shot: fetch a URL and return the navigate result. Closes immediately."""
    with Client() as ub:
        return ub.navigate(url)
