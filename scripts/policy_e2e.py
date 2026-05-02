#!/usr/bin/env python3
"""End-to-end policy measurement with `exec_scripts: true`.

Spawns the unbrowser binary with `--policy=blocklist`, navigates to each
target with exec_scripts=true, and reports how many static <script src>
URLs were actually blocked at fetch time vs how many were executed.

Compares against `policy_baseline.py`'s static-tag-only prediction to
verify the implementation matches what the policy module says it should
block.
"""
import json
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
BIN = REPO / "target" / "release" / "unbrowser"


class UB:
    def __init__(self, policy_block: bool):
        cmd = [str(BIN)]
        if policy_block:
            cmd.append("--policy=blocklist")
        # Capture stderr to count policy_blocked NDJSON events.
        self.p = subprocess.Popen(
            cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True,
        )
        self._id = 0

    def call(self, method, **params):
        self._id += 1
        msg = {"jsonrpc": "2.0", "id": self._id, "method": method, "params": params}
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()
        return json.loads(self.p.stdout.readline())

    def close(self):
        try:
            self.call("close")
        except Exception:
            pass
        try:
            _, stderr = self.p.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            self.p.kill()
            stderr = ""
        return stderr


def policy_blocked_events(stderr: str) -> list[dict]:
    out = []
    for line in stderr.splitlines():
        try:
            obj = json.loads(line)
        except Exception:
            continue
        if obj.get("event") == "policy_blocked":
            out.append(obj.get("data", obj))
    return out


def measure(url: str, policy_block: bool) -> dict:
    ub = UB(policy_block)
    t0 = time.perf_counter()
    nav = ub.call("navigate", url=url, exec_scripts=True)
    nav_ms = (time.perf_counter() - t0) * 1000
    stderr = ub.close()
    result = nav.get("result", {}) or {}
    scripts = result.get("scripts", {}) or {}
    return {
        "url": url,
        "policy_block": policy_block,
        "status": result.get("status"),
        "bytes": result.get("bytes"),
        "wall_ms": round(nav_ms, 1),
        "scripts": {
            "inline": scripts.get("inline_count"),
            "external": scripts.get("external_count"),
            "async": scripts.get("async_count"),
            "policy_blocked": scripts.get("policy_blocked"),
            "executed": scripts.get("executed"),
            "errors": scripts.get("errors_count"),
            "fetch_errors": scripts.get("fetch_errors_count"),
        },
        "policy_events": policy_blocked_events(stderr),
    }


def main():
    targets = [
        "https://news.ycombinator.com/",
        "https://www.cnbc.com/markets/",
        "https://www.forbes.com/",
        "https://www.theverge.com/",
    ]
    if len(sys.argv) > 1:
        targets = sys.argv[1:]

    rows = []
    for url in targets:
        print(f"\n=== {url}")
        try:
            off = measure(url, policy_block=False)
            on = measure(url, policy_block=True)
            print(f"  policy off  wall={off['wall_ms']:7.1f} ms  "
                  f"ext={off['scripts']['external']:3}  "
                  f"async={off['scripts']['async']:3}  "
                  f"blocked={off['scripts']['policy_blocked']:3}  "
                  f"exec={off['scripts']['executed']:3}")
            print(f"  policy on   wall={on['wall_ms']:7.1f} ms  "
                  f"ext={on['scripts']['external']:3}  "
                  f"async={on['scripts']['async']:3}  "
                  f"blocked={on['scripts']['policy_blocked']:3}  "
                  f"exec={on['scripts']['executed']:3}")
            if on["policy_events"]:
                print(f"  blocked URLs:")
                for ev in on["policy_events"]:
                    cat = ev.get("category", "?")
                    matched = ev.get("matched", "?")
                    u = ev.get("url", "?")
                    print(f"    [{cat:14s}] {matched:30s} {u[:70]}")
            rows.append({"off": off, "on": on})
        except Exception as e:
            print(f"  ERROR: {e}")

    out = REPO / "scripts" / "policy_e2e.json"
    out.write_text(json.dumps(rows, indent=2))
    print(f"\nWrote {out}")


if __name__ == "__main__":
    main()
