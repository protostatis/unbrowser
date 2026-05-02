#!/usr/bin/env python3
"""Smoke test for content-positive observers (PR after PR #7).

Three scenarios on one synthetic page:
  1. IntersectionObserver — observe a placeholder; on isIntersecting,
     replace its text. Verifies lazy-load patterns now work.
  2. ResizeObserver — observe an element; on first callback, set a
     marker. Verifies layout-conditioned UI proceeds.
  3. MutationObserver — observe body for childList changes. After it
     fires, append a marker that the observer detected the mutation.
     Verifies hydration paths waiting for "DOM has changed" callbacks
     now resolve.

The whole page is gated on these callbacks firing during settle.
"""
import http.server, json, socketserver, subprocess, threading
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
BIN = REPO / "target" / "release" / "unbrowser"

HTML = """<!DOCTYPE html>
<html><head><title>obs</title></head>
<body>
<div id="lazy">unlazy_initial</div>
<div id="resize-target">unresized_initial</div>
<div id="mutation-status">no_mutation</div>
<div id="results">starting</div>

<script>
  function setStatus(k, v) {
    document.getElementById(k).textContent = v;
  }

  // 1. IntersectionObserver — content-positive should fire isIntersecting=true
  var io = new IntersectionObserver(function(entries) {
    if (entries.length > 0 && entries[0].isIntersecting) {
      setStatus('lazy', 'lazy_loaded:ratio=' + entries[0].intersectionRatio);
    } else {
      setStatus('lazy', 'lazy_callback_no_intersect');
    }
  });
  io.observe(document.getElementById('lazy'));

  // 2. ResizeObserver — content-positive should fire once with viewport dims
  var ro = new ResizeObserver(function(entries) {
    if (entries.length > 0 && entries[0].contentRect) {
      var r = entries[0].contentRect;
      setStatus('resize-target', 'resized:' + r.width + 'x' + r.height);
    } else {
      setStatus('resize-target', 'resize_callback_no_rect');
    }
  });
  ro.observe(document.getElementById('resize-target'));

  // 3. MutationObserver — observe body for childList; trigger a mutation;
  // verify the observer fires and provides a childList record with
  // addedNodes populated. We also receive characterData records from
  // setStatus() updates, but we filter for the childList we care about.
  var sawChildList = false;
  var mo = new MutationObserver(function(records) {
    for (var i = 0; i < records.length; i++) {
      var r = records[i];
      if (r.type === 'childList' && r.addedNodes && r.addedNodes.length > 0) {
        sawChildList = true;
        var hasTarget = r.target ? 'has_target' : 'no_target';
        setStatus('mutation-status',
          'mutation:type=' + r.type +
          ':' + hasTarget +
          ':added=' + r.addedNodes.length);
        break;
      }
    }
  });
  mo.observe(document.body, { childList: true, subtree: true });

  // Trigger a mutation that the observer should pick up
  setTimeout(function() {
    var div = document.createElement('div');
    div.textContent = 'inserted_by_settle';
    div.id = 'inserted';
    document.body.appendChild(div);
  }, 0);

  // Final: write a results marker once everything settles
  setTimeout(function() {
    var lazy = document.getElementById('lazy').textContent;
    var resize = document.getElementById('resize-target').textContent;
    var mutation = document.getElementById('mutation-status').textContent;
    setStatus('results', lazy + ' | ' + resize + ' | ' + mutation);
  }, 50);
</script>
</body></html>
"""


class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path in ("/", "/index.html"):
            b = HTML.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)
        else:
            self.send_response(404); self.end_headers()
    def log_message(self, *_): pass


def main():
    httpd = socketserver.TCPServer(("127.0.0.1", 0), H)
    port = httpd.server_address[1]
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{port}/"
    print(f"server: {base}")

    p = subprocess.Popen([str(BIN)],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.DEVNULL, text=True)

    def call(method, **params):
        msg = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
        p.stdin.write(json.dumps(msg) + "\n"); p.stdin.flush()
        return json.loads(p.stdout.readline())

    nav = call("navigate", url=base, exec_scripts=True)
    res = nav.get("result", {}) or {}
    print(f"navigate status={res.get('status')}")

    for sel, label in [
        ("#lazy", "IntersectionObserver"),
        ("#resize-target", "ResizeObserver"),
        ("#mutation-status", "MutationObserver"),
        ("#results", "FINAL results"),
    ]:
        r = call("text", selector=sel).get("result", "")
        print(f"  {label:25s} {sel:18s} → {r!r}")

    final = call("text", selector="#results").get("result", "")
    call("close")
    p.communicate(timeout=2)
    httpd.shutdown()

    print(f"\nfinal #results: {final!r}")
    print()

    ok = True
    if "lazy_loaded:ratio=1" not in final:
        print(f"FAIL: IntersectionObserver did not fire isIntersecting=1")
        ok = False
    else:
        print("PASS: IntersectionObserver fired with isIntersecting=true")
    if "resized:1280x800" not in final:
        print(f"FAIL: ResizeObserver did not fire with viewport dims")
        ok = False
    else:
        print("PASS: ResizeObserver fired with viewport dims")
    if "mutation:type=childList" not in final or "has_target" not in final or "added=1" not in final:
        print(f"FAIL: MutationObserver did not fire with childList record")
        ok = False
    else:
        print("PASS: MutationObserver fired with childList record (target + addedNodes)")

    print()
    print("ALL PASS" if ok else "FAILURES")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
