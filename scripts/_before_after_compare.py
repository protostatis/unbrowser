#!/usr/bin/env python3
"""Before/after comparison: 8b1e5e9 (pre-PR-#2 baseline) vs HEAD.

Runs each binary against a small site corpus and tabulates the
deltas — wall-clock, scripts executed, blockmap completeness, network
captured (after only), prefit applied (after only).

Both binaries get exec_scripts=true. The "after" binary additionally
gets --policy=blocklist since the policy framework is its main
addition. So this is "before, no policy" vs "after, fully on" — the
realistic comparison since the policy is opt-in pre-PR-#2 (the flag
doesn't exist there).

This is a one-off measurement script, not a regression test —
prefixed with underscore so it doesn't accumulate in scripts/.
"""
import json
import select
import subprocess
import time
from pathlib import Path
from typing import Optional

NAV_TIMEOUT_S = 30  # per-navigate budget; bigger than CNBC's worst case

BEFORE_BIN = Path("/tmp/unbrowser-before/target/release/unbrowser")
AFTER_BIN = Path("/Users/zhiminzou/Projects/unchained_browser/target/release/unbrowser")

SITES = [
    # Prefit-bundle domains — verify each entry fires.
    ("https://news.ycombinator.com/", "HN"),
    ("https://en.wikipedia.org/wiki/Bayesian_inference", "Wikipedia"),
    ("https://www.cnbc.com/markets/", "CNBC"),
    ("https://www.npmjs.com/package/react", "npm"),
    ("https://github.com/anthropics/anthropic-sdk-python", "GitHub"),
    ("https://www.reddit.com/r/programming/", "Reddit"),
    ("https://www.youtube.com/feed/trending", "YouTube"),
    ("https://polymarket.com/", "Polymarket"),
    ("https://kalshi.com/markets", "Kalshi"),
    ("https://www.zillow.com/homes/for_rent/", "Zillow"),
    # Off-bundle — exercise the framework_priors fallback path.
    ("https://arxiv.org/abs/2104.13478", "arxiv"),
    ("https://www.nytimes.com/", "NYT"),
    ("https://www.theverge.com/", "Verge"),
    ("https://stackoverflow.com/questions/11227809/why-is-processing-a-sorted-array-faster-than-processing-an-unsorted-array", "StackOverflow"),
    ("https://medium.com/", "Medium"),
]


def run(binary: Path, url: str, policy_on: bool) -> dict:
    cmd = [str(binary)]
    if policy_on:
        cmd.append("--policy=blocklist")
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, text=True)
    t0 = time.perf_counter()
    p.stdin.write(json.dumps({
        "jsonrpc": "2.0", "id": 1, "method": "navigate",
        "params": {"url": url, "exec_scripts": True}
    }) + "\n")
    p.stdin.flush()
    deadline = time.time() + NAV_TIMEOUT_S
    line = None
    while time.time() < deadline:
        r, _, _ = select.select([p.stdout], [], [], 0.5)
        if r:
            line = p.stdout.readline()
            break
    wall_ms = (time.perf_counter() - t0) * 1000
    if line is None:
        # Hard hang — kill, capture stderr, return a hang sentinel.
        p.kill()
        try:
            _, stderr = p.communicate(timeout=5)
        except Exception:
            stderr = ""
        events = []
        for ln in stderr.splitlines():
            try:
                events.append(json.loads(ln))
            except Exception:
                pass
        return {"wall_ms": wall_ms, "result": {}, "error": "TIMEOUT",
                "hang": True, "events": events}
    nav = json.loads(line)
    p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 2, "method": "close", "params": {}}) + "\n")
    p.stdin.flush()
    try:
        _, stderr = p.communicate(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill()
        _, stderr = p.communicate(timeout=5)
    res = nav.get("result", {}) or {}
    err = nav.get("error")
    events = []
    for line in stderr.splitlines():
        try:
            events.append(json.loads(line))
        except Exception:
            pass
    return {
        "wall_ms": wall_ms,
        "result": res,
        "error": err,
        "hang": False,
        "events": events,
    }


def summarize(label: str, r: dict) -> dict:
    res = r["result"]
    scripts = res.get("scripts") or {}
    blockmap = res.get("blockmap") or {}
    interactives = blockmap.get("interactives") or {}
    network = res.get("network_stores") or {}
    extract = res.get("extract")
    events = r["events"]

    prefit_applied = next((e["data"] for e in events if e.get("event") == "prefit_applied"), None)
    cache_events = [e["data"] for e in events if e.get("event") == "bytecode_cache"]

    return {
        "label": label,
        "status": res.get("status"),
        "bytes": res.get("bytes"),
        "wall_ms": round(r["wall_ms"], 0),
        "scripts_executed": scripts.get("executed"),
        "scripts_blocked": scripts.get("policy_blocked"),
        "scripts_async": scripts.get("async_count"),
        "settle_reason_dcl": (scripts.get("settle_after_dcl") or {}).get("reason"),
        "interactives_links": interactives.get("links") if isinstance(interactives, dict) else None,
        "interactives_buttons": interactives.get("buttons") if isinstance(interactives, dict) else None,
        "network_captured": network.get("count") if network else None,
        "extract_present": bool(extract),
        "prefit_domain": prefit_applied.get("domain") if prefit_applied else None,
        "bytecode_hits": sum(1 for e in cache_events if e.get("hit")),
        "bytecode_misses": sum(1 for e in cache_events if not e.get("hit")),
    }


def main():
    print(f"BEFORE binary: {BEFORE_BIN} (commit 8b1e5e9 — pre-PR-#2)", flush=True)
    print(f"AFTER  binary: {AFTER_BIN} (HEAD — 12 PRs landed)", flush=True)
    print(f"NAV_TIMEOUT_S: {NAV_TIMEOUT_S}", flush=True)
    print(flush=True)

    rows = []
    for url, name in SITES:
        print(f"--- {name} ({url}) ---", flush=True)
        before = run(BEFORE_BIN, url, policy_on=False)
        after = run(AFTER_BIN, url, policy_on=True)
        # second after run for warm-cache delta — skip if cold hung
        if after.get("hang"):
            after2 = {"wall_ms": 0, "result": {}, "error": "SKIPPED", "hang": True, "events": []}
        else:
            after2 = run(AFTER_BIN, url, policy_on=True)

        b = summarize("before", before)
        a = summarize("after-cold", after)
        a2 = summarize("after-warm", after2)
        b["hang"] = before.get("hang", False)
        a["hang"] = after.get("hang", False)
        a2["hang"] = after2.get("hang", False)
        rows.append((name, b, a, a2))

        for r in (b, a, a2):
            tag = " HANG" if r.get("hang") else ""
            print(f"  {r['label']:12s}  wall={r['wall_ms']:6.0f}ms{tag}  status={r['status']}  "
                  f"scripts={r['scripts_executed']}/{r['scripts_blocked']}  "
                  f"links={r['interactives_links']}  "
                  f"network={r['network_captured']}  extract={r['extract_present']}", flush=True)

    def cell(v, hang):
        if hang:
            return "HANG"
        return f"{int(v):>5d}" if v is not None else "  -  "

    # Final compact table
    print(flush=True)
    print("=" * 130, flush=True)
    print(f"{'site':<10s}  {'wall_ms (b/cold/warm)':<26s}  {'scripts_exec':<14s}  {'blocked':<8s}  "
          f"{'links':<6s}  {'net_cap':<8s}  {'extract':<8s}  {'prefit':<14s}", flush=True)
    print("-" * 130, flush=True)
    for name, b, a, a2 in rows:
        wall = f"{cell(b['wall_ms'], b.get('hang')):>5s}/{cell(a['wall_ms'], a.get('hang')):>5s}/{cell(a2['wall_ms'], a2.get('hang')):>5s}"
        scripts = f"{b['scripts_executed']}/{a['scripts_executed']}/{a2['scripts_executed']}"
        blocked = f"{b['scripts_blocked'] or 0}→{a['scripts_blocked'] or 0}"
        links_b = b['interactives_links'] or 0
        links_a = a['interactives_links'] or 0
        links = f"{links_b}→{links_a}"
        netcap = f"{a['network_captured'] or 0} (a)"
        extract = f"{'no' if not b['extract_present'] else 'yes'}→{'yes' if a['extract_present'] else 'no'}"
        prefit = a['prefit_domain'] or "-"
        print(f"{name:<10s}  {wall:<26s}  {scripts:<14s}  {blocked:<8s}  {links:<6s}  "
              f"{netcap:<8s}  {extract:<8s}  {prefit:<14s}", flush=True)

    # Wall-clock deltas summary
    print(flush=True)
    print("Wall-clock deltas (negative = faster):", flush=True)
    for name, b, a, a2 in rows:
        if a.get("hang") or b.get("hang"):
            print(f"  {name:<10s}  cold-vs-before: HANG  (one of the binaries did not return within {NAV_TIMEOUT_S}s)", flush=True)
            continue
        cold_delta = a['wall_ms'] - b['wall_ms']
        warm_delta = a2['wall_ms'] - b['wall_ms'] if not a2.get("hang") else None
        warm_vs_cold = a2['wall_ms'] - a['wall_ms'] if not a2.get("hang") else None
        wd = f"{warm_delta:+6.0f}ms" if warm_delta is not None else "    -  "
        wvc = f"{warm_vs_cold:+6.0f}ms" if warm_vs_cold is not None else "    -  "
        print(f"  {name:<10s}  cold-vs-before: {cold_delta:+6.0f}ms  warm-vs-before: {wd}  warm-vs-cold: {wvc}", flush=True)


if __name__ == "__main__":
    main()
