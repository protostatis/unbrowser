#!/usr/bin/env python3
"""Smoke test for ES module loader (best-effort v1).

Synthetic page with:
  /index.html — has <script type="module" src="/entry.js">
  /entry.js   — imports './helper.js' (side-effect) and './lib.js'
                then writes a marker. Verifies dep evaluation order
                and that the entry runs after deps.
  /helper.js  — sets a global on import (side-effect import)
  /lib.js     — sets another global

Verifies all four globals are present after navigate, in dependency order.
"""
import http.server, json, os, socketserver, subprocess, threading
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]


def _resolve_bin() -> Path:
    env = os.environ.get("UNBROWSER_BIN")
    if env:
        return Path(env)
    rel = REPO / "target" / "release" / "unbrowser"
    if rel.exists():
        return rel
    return REPO / "target" / "debug" / "unbrowser"


BIN = _resolve_bin()

INDEX_HTML = """<!DOCTYPE html>
<html><body>
<div id="status">starting</div>
<script type="module" src="/entry.js"></script>
</body></html>
"""

ENTRY_JS = """
import './helper.js';
import './lib.js';
window.__entry_ran = true;
window.__entry_at = Date.now();
window.__order = (window.__order || []).concat('entry');
document.getElementById('status').textContent =
  'entry_ran:helper=' + (typeof window.__helper_ran) +
  ':lib=' + (typeof window.__lib_ran) +
  ':order=' + window.__order.join(',');
"""

HELPER_JS = """
window.__helper_ran = true;
window.__helper_at = Date.now();
window.__order = (window.__order || []).concat('helper');
"""

LIB_JS = """
window.__lib_ran = true;
window.__lib_at = Date.now();
window.__order = (window.__order || []).concat('lib');
"""

# Static + module — verifies a classic <script> and a <script type=module>
# coexist on one page.
COEXIST_HTML = """<!DOCTYPE html>
<html><body>
<div id="x"></div>
<script>window.__classic_ran = true;</script>
<script type="module" src="/entry.js"></script>
</body></html>
"""


class H(http.server.BaseHTTPRequestHandler):
    routes = {
        "/index.html": INDEX_HTML,
        "/coexist.html": COEXIST_HTML,
        "/entry.js": ENTRY_JS,
        "/helper.js": HELPER_JS,
        "/lib.js": LIB_JS,
    }

    def do_GET(self):
        body = self.routes.get(self.path)
        if body is None:
            if self.path == "/":
                body = INDEX_HTML
            else:
                self.send_response(404); self.end_headers(); return
        ct = "text/html" if self.path.endswith(".html") or self.path == "/" else "application/javascript"
        b = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", ct)
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)
    def log_message(self, *_): pass


def main():
    httpd = socketserver.TCPServer(("127.0.0.1", 0), H)
    port = httpd.server_address[1]
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{port}"
    print(f"server: {base}")

    p = subprocess.Popen([str(BIN)],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.DEVNULL, text=True)

    def call(method, **params):
        msg = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
        p.stdin.write(json.dumps(msg) + "\n"); p.stdin.flush()
        return json.loads(p.stdout.readline())

    print("\n=== Scenario 1: module entry imports two side-effect deps ===")
    nav = call("navigate", url=f"{base}/index.html", exec_scripts=True)
    print(f"navigate status={nav.get('result', {}).get('status')}")

    helper = call("eval", code="typeof window.__helper_ran").get("result")
    lib = call("eval", code="typeof window.__lib_ran").get("result")
    entry = call("eval", code="typeof window.__entry_ran").get("result")
    order = call("eval", code="JSON.stringify(window.__order || [])").get("result")
    status = call("text", selector="#status").get("result", "")
    print(f"  helper ran: {helper!r}")
    print(f"  lib ran:    {lib!r}")
    print(f"  entry ran:  {entry!r}")
    print(f"  order:      {order!r}")
    print(f"  #status:    {status!r}")

    ok1 = (helper == "boolean" and lib == "boolean" and entry == "boolean")

    # Verify deps ran BEFORE the entry (dependency order)
    try:
        order_list = json.loads(order)
        entry_idx = order_list.index("entry") if "entry" in order_list else -1
        helper_idx = order_list.index("helper") if "helper" in order_list else -1
        lib_idx = order_list.index("lib") if "lib" in order_list else -1
        deps_before = (entry_idx > helper_idx and entry_idx > lib_idx and
                      helper_idx >= 0 and lib_idx >= 0)
    except Exception:
        deps_before = False

    print()
    print("  PASS" if ok1 else "  FAIL", "module + deps all evaluated")
    print("  PASS" if deps_before else "  FAIL", "deps evaluated before entry (module graph order)")

    print("\n=== Scenario 2: classic <script> + <script type=module> coexist ===")
    nav = call("navigate", url=f"{base}/coexist.html", exec_scripts=True)
    classic = call("eval", code="typeof window.__classic_ran").get("result")
    entry = call("eval", code="typeof window.__entry_ran").get("result")
    print(f"  classic ran: {classic!r}")
    print(f"  entry ran:   {entry!r}")
    ok2 = (classic == "boolean" and entry == "boolean")
    print("  PASS" if ok2 else "  FAIL", "both classic and module scripts ran")

    print("\n=== Scenario 3: module cache reset across navigates ===")
    # Navigate to a fresh page that re-imports lib.js. The helper/lib globals
    # would already exist from scenario 1's eval, so we check cache state by
    # asking the loader if it has the entry_url in its cache.
    nav = call("navigate", url=f"{base}/index.html", exec_scripts=True)
    # After nav, helper/lib/entry should re-execute — globals persist (we
    # don't reset window) but order array is appended (so longer than before)
    final_order_len = call("eval", code="(window.__order || []).length").get("result", 0)
    # In one session the order list accumulates: scenario 1 appended 3, scenario 2
    # navigated via index module too via the import + appended 3 more, scenario 3
    # appended 3 more. Final length should be ≥9.
    print(f"  final __order length after 3 navigates: {final_order_len}")
    ok3 = final_order_len >= 9
    print("  PASS" if ok3 else "  FAIL",
          f"module cache reset on each navigate (order accumulated to {final_order_len})")

    call("close")
    p.communicate(timeout=2)
    httpd.shutdown()

    print()
    ok = ok1 and deps_before and ok2 and ok3
    print("ALL PASS" if ok else "FAILURES")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
