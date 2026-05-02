#!/usr/bin/env python3
"""Measure script load patterns on real sites to drive policy MVP scope.

Spawns the unbrowser binary, navigates to a target, queries all
<script src> URLs, classifies them via `unbrowser policy-check` (the
Rust policy module is the single source of truth), and reports what a
blocklist policy would skip.
"""
import json
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path
from urllib.parse import urljoin, urlparse

REPO = Path(__file__).resolve().parents[1]
BIN = REPO / "target" / "release" / "unbrowser"


def classify_urls(urls: list[str]) -> list[dict]:
    """Call `unbrowser policy-check` for a batch of URLs.

    Output format: <decision>\t<category>\t<matched>\t<host>\t<url> per line.
    """
    if not urls:
        return []
    out = subprocess.run(
        [str(BIN), "policy-check", *urls],
        check=True, capture_output=True, text=True,
    ).stdout
    rows = []
    for line in out.strip().splitlines():
        parts = line.split("\t")
        if len(parts) < 5:
            continue
        decision, category, matched, host, url = parts[:5]
        rows.append({
            "url": url, "host": host, "decision": decision,
            "category": category if decision == "block" else None,
            "matched": matched if decision == "block" else None,
        })
    return rows


class UB:
    def __init__(self):
        self.p = subprocess.Popen(
            [str(BIN)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True,
        )
        self._id = 0

    def call(self, method, **params):
        self._id += 1
        msg = {"jsonrpc": "2.0", "id": self._id, "method": method, "params": params}
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()
        line = self.p.stdout.readline()
        return json.loads(line)

    def close(self):
        try:
            self.call("close")
        except Exception:
            pass
        self.p.wait(timeout=2)


def measure(url: str) -> dict:
    ub = UB()
    try:
        t0 = time.perf_counter()
        nav = ub.call("navigate", url=url)
        nav_ms = (time.perf_counter() - t0) * 1000
        nav_result = nav.get("result", {})
        status = nav_result.get("status")
        bytes_ = nav_result.get("bytes")

        all_scripts = ub.call("query", selector="script").get("result", [])
        src_scripts = ub.call("query", selector="script[src]").get("result", [])

        full_urls = []
        for s in src_scripts:
            src = s.get("attrs", {}).get("src", "")
            if not src:
                continue
            full_urls.append(urljoin(url, src))

        decisions = classify_urls(full_urls)
        blocked = [d for d in decisions if d["decision"] == "block"]
        by_category = Counter(d["category"] for d in blocked)
        by_pattern = Counter(d["matched"] for d in blocked)
        all_hosts = Counter(d["host"] for d in decisions)

        return {
            "url": url,
            "status": status,
            "bytes": bytes_,
            "nav_ms": round(nav_ms, 1),
            "total_scripts": len(all_scripts),
            "src_scripts": len(src_scripts),
            "inline_scripts": len(all_scripts) - len(src_scripts),
            "unique_hosts": len(set(d["host"] for d in decisions)),
            "would_block": len(blocked),
            "would_block_pct": round(100 * len(blocked) / max(1, len(src_scripts)), 1),
            "by_category": dict(by_category),
            "blocked_patterns": dict(by_pattern),
            "all_hosts": all_hosts.most_common(20),
            "decisions": decisions,
        }
    finally:
        ub.close()


def main():
    targets = [
        "https://news.ycombinator.com/",
        "https://www.cnbc.com/markets/",
        "https://www.forbes.com/",
        "https://en.wikipedia.org/wiki/Bayesian_inference",
        "https://www.theverge.com/",
    ]
    if len(sys.argv) > 1:
        targets = sys.argv[1:]

    results = []
    for url in targets:
        print(f"\n=== {url}", flush=True)
        try:
            r = measure(url)
            results.append(r)
            print(f"  status={r['status']} bytes={r['bytes']} nav_ms={r['nav_ms']}")
            print(f"  scripts: {r['src_scripts']} src + {r['inline_scripts']} inline = {r['total_scripts']} total")
            print(f"  unique_hosts: {r['unique_hosts']}")
            print(f"  would_block: {r['would_block']} ({r['would_block_pct']}%)")
            if r['by_category']:
                print(f"  by category:")
                for c, n in sorted(r['by_category'].items(), key=lambda x: -x[1]):
                    print(f"    {n:3d}  {c}")
            if r['blocked_patterns']:
                print(f"  matched patterns:")
                for p, n in sorted(r['blocked_patterns'].items(), key=lambda x: -x[1]):
                    print(f"    {n:3d}  {p}")
            blocked_hosts = {d["host"] for d in r["decisions"] if d["decision"] == "block"}
            print(f"  top all hosts:")
            for h, n in r['all_hosts'][:10]:
                marker = " [BLOCK]" if h in blocked_hosts else ""
                print(f"    {n:3d}  {h}{marker}")
        except Exception as e:
            print(f"  ERROR: {e}")
            results.append({"url": url, "error": str(e)})

    out = REPO / "scripts" / "policy_baseline.json"
    out.write_text(json.dumps(results, indent=2))
    print(f"\nWrote {out}")


if __name__ == "__main__":
    main()
