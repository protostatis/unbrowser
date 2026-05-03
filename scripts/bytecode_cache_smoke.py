#!/usr/bin/env python3
"""Smoke test for bytecode cache.

Two scenarios:
  1. Cold cache: first navigate emits bytecode_cache events with hit=false.
     Subsequent navigate (with same scripts) should emit hit=true and
     have measurably faster script-eval phase.
  2. Cache invalidation: setting UNBROWSER_NO_BYTECODE_CACHE=1 disables
     caching — no bytecode_cache events emitted at all.

Uses a local HTTP server with a moderately-sized synthetic JS bundle so
the cache effect is detectable without external network jitter.
"""
import http.server, json, os, shutil, socketserver, subprocess, tempfile, threading, time
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

# Synthetic ~50 KB script — roughly the size of a small framework chunk.
# The cache benefit grows with script size, but even at this scale the
# parse cost is measurable.
LARGE_SCRIPT = "// generated\n" + "\n".join(
    f"window.__gen_{i} = (function() {{ var x = {i}; return function() {{ return x * 2; }}; }})();"
    for i in range(2000)
)

HTML = f"""<!DOCTYPE html>
<html><body>
<div id="x">page</div>
<script>{LARGE_SCRIPT}</script>
<script src="/bundle.js"></script>
</body></html>
"""


class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path in ("/", "/index.html"):
            b = HTML.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html"); self.send_header("Content-Length", str(len(b)))
            self.end_headers(); self.wfile.write(b)
        elif self.path == "/bundle.js":
            b = LARGE_SCRIPT.encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/javascript"); self.send_header("Content-Length", str(len(b)))
            self.end_headers(); self.wfile.write(b)
        else:
            self.send_response(404); self.end_headers()
    def log_message(self, *_): pass


def navigate(url, env_overrides=None):
    """Run a single navigate, return (events, navigate_result)."""
    env = dict(os.environ)
    if env_overrides:
        env.update(env_overrides)
    p = subprocess.Popen([str(BIN)],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, text=True, env=env)
    msg = json.dumps({"jsonrpc":"2.0","id":1,"method":"navigate",
                      "params":{"url":url, "exec_scripts":True}})
    p.stdin.write(msg + "\n"); p.stdin.flush()
    nav = json.loads(p.stdout.readline())
    p.stdin.write(json.dumps({"jsonrpc":"2.0","id":2,"method":"close","params":{}}) + "\n")
    p.stdin.flush()
    _, stderr = p.communicate(timeout=5)
    events = []
    for line in stderr.splitlines():
        try: events.append(json.loads(line))
        except Exception: pass
    return events, nav.get("result", {}) or {}


def cache_events(events):
    return [e["data"] for e in events if e.get("event") == "bytecode_cache"]


def main():
    httpd = socketserver.TCPServer(("127.0.0.1", 0), H)
    port = httpd.server_address[1]
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{port}/"
    print(f"server: {base}")

    # Use a fresh, isolated cache dir for this test
    cache_dir = tempfile.mkdtemp(prefix="unb_bcache_test_")
    print(f"cache dir: {cache_dir}")
    print(f"script size: {len(LARGE_SCRIPT)} bytes")

    try:
        env = {"UNBROWSER_BYTECODE_CACHE": cache_dir}

        print("\n=== Scenario 1: cold cache (first navigate) ===")
        t0 = time.perf_counter()
        events1, res1 = navigate(base, env)
        cold_ms = (time.perf_counter() - t0) * 1000
        ce1 = cache_events(events1)
        print(f"  wall: {cold_ms:.0f}ms")
        print(f"  bytecode_cache events: {len(ce1)}")
        misses_cold = sum(1 for e in ce1 if not e.get("hit"))
        hits_cold = sum(1 for e in ce1 if e.get("hit"))
        print(f"    hit={hits_cold} miss={misses_cold}")
        for e in ce1[:3]:
            print(f"    {json.dumps(e)[:120]}")

        print("\n=== Scenario 2: warm cache (second navigate, same scripts) ===")
        t0 = time.perf_counter()
        events2, res2 = navigate(base, env)
        warm_ms = (time.perf_counter() - t0) * 1000
        ce2 = cache_events(events2)
        misses_warm = sum(1 for e in ce2 if not e.get("hit"))
        hits_warm = sum(1 for e in ce2 if e.get("hit"))
        print(f"  wall: {warm_ms:.0f}ms")
        print(f"  bytecode_cache events: {len(ce2)} (hit={hits_warm} miss={misses_warm})")

        print("\n=== Scenario 3: cache disabled (UNBROWSER_NO_BYTECODE_CACHE=1) ===")
        env_off = dict(env); env_off["UNBROWSER_NO_BYTECODE_CACHE"] = "1"
        events3, res3 = navigate(base, env_off)
        ce3 = cache_events(events3)
        print(f"  bytecode_cache events: {len(ce3)} (expected 0)")

        # List cache contents
        files = list(Path(cache_dir).rglob("*.qbc"))
        total_bytes = sum(f.stat().st_size for f in files)
        print(f"\ncache contents: {len(files)} files, {total_bytes} bytes total")

        print()
        ok = True
        if misses_cold < 1:
            print(f"FAIL: cold pass should miss at least 1 (got {misses_cold})")
            ok = False
        else:
            print(f"PASS: cold pass missed {misses_cold} entries")
        if hits_warm < 1:
            print(f"FAIL: warm pass should hit at least 1 (got {hits_warm})")
            ok = False
        else:
            print(f"PASS: warm pass hit {hits_warm} entries")
        if misses_warm > 0:
            print(f"WARN: warm pass had {misses_warm} misses (expected 0)")
        if len(ce3) > 0:
            print(f"FAIL: disabled cache should emit 0 events (got {len(ce3)})")
            ok = False
        else:
            print(f"PASS: disabled cache emitted 0 events")
        if len(files) < 1:
            print(f"FAIL: cache dir should have ≥1 file (got {len(files)})")
            ok = False
        else:
            print(f"PASS: cache dir has {len(files)} bytecode files")

        print()
        print("ALL PASS" if ok else "FAILURES")
        return 0 if ok else 1
    finally:
        httpd.shutdown()
        shutil.rmtree(cache_dir, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
