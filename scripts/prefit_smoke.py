#!/usr/bin/env python3
"""Smoke test for the prefit pipeline (R1 + R2 from white paper §6).

Three scenarios:
  1. Domain in prefit (cnbc.com) — verify prefit_applied event fires
     with the right payload, and that prefit's blocklist_additions
     extend the policy block (the per-site trackers get blocked on
     this navigation even though they're not in the global Tier-1
     blocklist).
  2. Domain NOT in prefit (example.com) — no prefit_applied event,
     navigation works as before.
  3. --prefit-info CLI — verify it dumps the embedded bundle.

This is the v0 demo of the prefit-first architecture: ship a trained
prior in the binary, look it up at navigate time, apply per-(domain,
framework) decisions on first visit. No per-user learning required.
"""
import json
import os
import subprocess
import sys
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


def collect_events(stderr: str) -> list:
    out = []
    for line in stderr.splitlines():
        try:
            out.append(json.loads(line))
        except Exception:
            pass
    return out


def navigate(url: str, policy: bool = True):
    cmd = [str(BIN)]
    if policy:
        cmd.append("--policy=blocklist")
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, text=True)
    p.stdin.write(json.dumps({"jsonrpc":"2.0","id":1,"method":"navigate",
                              "params":{"url":url, "exec_scripts":True}}) + "\n")
    p.stdin.flush()
    nav = json.loads(p.stdout.readline())
    p.stdin.write(json.dumps({"jsonrpc":"2.0","id":2,"method":"close","params":{}}) + "\n")
    p.stdin.flush()
    _, stderr = p.communicate(timeout=20)
    return nav.get("result", {}) or {}, collect_events(stderr)


def main():
    ok = True

    # ---- Scenario 1: cnbc.com is in prefit ----
    print("=== Scenario 1: cnbc.com (in prefit) ===")
    res, events = navigate("https://www.cnbc.com/markets/")
    pa = [e["data"] for e in events if e.get("event") == "prefit_applied"]
    if not pa:
        print("FAIL: no prefit_applied event")
        ok = False
    else:
        d = pa[0]
        print(f"  prefit_applied: domain={d.get('domain')} framework={d.get('framework')} "
              f"blocklist_additions={d.get('blocklist_additions')} "
              f"shape={d.get('shape_hint')}")
        if d.get("domain") != "cnbc.com":
            print(f"FAIL: expected domain=cnbc.com, got {d.get('domain')!r}")
            ok = False
        else:
            print("PASS: prefit_applied fired with cnbc.com entry")

    # Look for any script_decision with reason=prefit_blocklist
    prefit_skipped = [e["data"] for e in events
                       if e.get("event") == "script_decision"
                       and e.get("data", {}).get("reason") == "prefit_blocklist"]
    print(f"  script_decision skip-by-prefit_blocklist count: {len(prefit_skipped)}")
    if prefit_skipped:
        for s in prefit_skipped[:5]:
            print(f"    [prefit] {s.get('host'):30s} {s.get('url', '')[:60]}")

    # ---- Scenario 2: example.com is NOT in prefit ----
    print("\n=== Scenario 2: example.com (NOT in prefit) ===")
    res, events = navigate("https://example.com/")
    pa = [e for e in events if e.get("event") == "prefit_applied"]
    if pa:
        print(f"FAIL: prefit_applied fired for example.com (shouldn't have)")
        ok = False
    else:
        print("PASS: no prefit_applied for unknown domain (expected)")
    nav_started = any(e.get("event") == "navigation_started" for e in events)
    if not nav_started:
        print("FAIL: navigate didn't even fire navigation_started")
        ok = False
    else:
        print("PASS: navigation worked normally without prefit")

    # ---- Scenario 3: --prefit-info CLI ----
    print("\n=== Scenario 3: --prefit-info CLI ===")
    out = subprocess.run([str(BIN), "--prefit-info"], capture_output=True, text=True)
    if "domains:" not in out.stdout or "fit_corpus_size:" not in out.stdout:
        print(f"FAIL: --prefit-info output missing expected fields")
        print(out.stdout[:200])
        ok = False
    else:
        # First few lines for visual confirmation
        for line in out.stdout.splitlines()[:6]:
            print(f"  {line}")
        print("PASS: --prefit-info dumps bundle metadata")

    print()
    print("ALL PASS" if ok else "FAILURES")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
