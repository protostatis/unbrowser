#!/usr/bin/env python3
"""Smoke test for the settle reason field.

Three scenarios:
  1. Static SSR (HN) — should settle with reason=idle (nothing in flight)
  2. Synthetic infinite-rAF page — should settle with reason=budget_exhausted
     (we keep scheduling timers; rAF is aliased to setTimeout(cb, 16))
  3. Synthetic empty page — also reason=idle

Verifies the field is present in scripts.settle_after_dcl /
scripts.settle_after_load AND back-compat timed_out is consistent.
"""
import http.server, json, socketserver, subprocess, threading
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
BIN = REPO / "target" / "release" / "unbrowser"

EMPTY_HTML = "<!DOCTYPE html><html><body><div>quiet</div></body></html>"

# Schedules a rAF that schedules itself indefinitely → never goes idle.
# Settle should hit its budget cap (2000ms after_dcl, 1500ms after_load).
INFINITE_RAF_HTML = """<!DOCTYPE html>
<html><body><div id="x">looping</div>
<script>
function loop() {
  document.getElementById('x').textContent = 'tick';
  requestAnimationFrame(loop);
}
loop();
</script>
</body></html>
"""


class H(http.server.BaseHTTPRequestHandler):
    routes = {}
    def do_GET(self):
        body = self.routes.get(self.path, "<html><body>404</body></html>").encode()
        status = 200 if self.path in self.routes else 404
        self.send_response(status)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *_): pass


def navigate(url, exec_scripts=True):
    p = subprocess.Popen([str(BIN)],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.DEVNULL, text=True)
    msg = json.dumps({"jsonrpc":"2.0","id":1,"method":"navigate",
                      "params":{"url":url, "exec_scripts":exec_scripts}})
    p.stdin.write(msg + "\n"); p.stdin.flush()
    nav = json.loads(p.stdout.readline())
    p.stdin.write(json.dumps({"jsonrpc":"2.0","id":2,"method":"close","params":{}}) + "\n")
    p.stdin.flush()
    p.communicate(timeout=2)
    return nav.get("result", {})


def settle_info(nav_result, key):
    s = (nav_result.get("scripts") or {}).get(key) or {}
    return s.get("reason"), s.get("timed_out"), s.get("elapsed_ms")


def check(label, expected_reason, reason, timed_out, elapsed_ms, expected_timed_out):
    ok = reason == expected_reason and timed_out == expected_timed_out
    status = "PASS" if ok else "FAIL"
    print(f"  {status}  {label}: reason={reason!r} timed_out={timed_out} elapsed={elapsed_ms}ms (expected reason={expected_reason!r} timed_out={expected_timed_out})")
    return ok


def main():
    H.routes = {
        "/empty.html": EMPTY_HTML,
        "/loop.html": INFINITE_RAF_HTML,
    }
    httpd = socketserver.TCPServer(("127.0.0.1", 0), H)
    port = httpd.server_address[1]
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{port}"

    ok = True

    # 1. Empty page → idle
    print("=== Scenario 1: empty page (expected: idle) ===")
    res = navigate(f"{base}/empty.html")
    r, to, ms = settle_info(res, "settle_after_dcl")
    ok &= check("settle_after_dcl", "idle", r, to, ms, False)
    r, to, ms = settle_info(res, "settle_after_load")
    ok &= check("settle_after_load", "idle", r, to, ms, False)

    # 2. Infinite rAF — should hit a budget cap. Either reason is valid:
    # max_iters fires first when the rAF chain schedules a fresh timer per
    # iter (100-iter cap × ~16ms wait < 2000ms budget); budget_exhausted
    # fires first if the per-iter wait stretches.
    print("\n=== Scenario 2: infinite rAF (expected: max_iters OR budget_exhausted) ===")
    res = navigate(f"{base}/loop.html")
    for phase in ("settle_after_dcl", "settle_after_load"):
        r, to, ms = settle_info(res, phase)
        accepted = r in ("max_iters", "budget_exhausted")
        status = "PASS" if accepted and to is True else "FAIL"
        print(f"  {status}  {phase}: reason={r!r} timed_out={to} elapsed={ms}ms")
        if not (accepted and to is True):
            ok = False

    # 3. HN — real-site sanity
    print("\n=== Scenario 3: news.ycombinator.com (real-site sanity, expect idle) ===")
    res = navigate("https://news.ycombinator.com/")
    r, to, ms = settle_info(res, "settle_after_dcl")
    print(f"  HN settle_after_dcl: reason={r!r} timed_out={to} elapsed={ms}ms")
    # HN is static SSR with very few scripts — should idle quickly
    if r != "idle":
        print(f"  WARN  HN didn't reach idle (network slow?). Not failing the test.")

    httpd.shutdown()
    print()
    print("ALL PASS" if ok else "FAILURES")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
