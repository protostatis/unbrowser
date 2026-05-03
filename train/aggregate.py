#!/usr/bin/env python3
"""T2 — aggregate Phase A events into per-domain decision parameters.

Reads T1's `train/runs/{ts}/{domain}/*.events.jsonl` files and produces
`train/aggregates/{domain}.json` — per-domain decision parameters
suitable for T3 (pack.py) to bundle into prefit/v1.bundle.json.

v0 aggregation:
  - blocklist_additions: hosts that appeared as `policy_blocked` events
    but aren't in the global Tier-1 blocklist (i.e. site-specific
    trackers worth adding to this domain's prefit)
  - settle_distribution: percentiles of `policy_trace.elapsed_ms` for
    successful navigations (placeholder p50/p90/p95)
  - shape_hint: derived from blockmap.density.likely_js_filled (when
    available in the event stream — currently a stub)
  - framework: derived from inspecting the page's main scripts (also
    a stub for v0 — manually annotated in the hand-curated bundle)

The Bayesian posterior fitting (Beta-Binomial updates per spec §4.2)
is deferred to a v1 aggregator. This v0 just produces the bundle
shape so the runtime pipeline can be demonstrated end-to-end.

Usage:
  python3 train/aggregate.py [--runs-dir DIR] [--out DIR]

Defaults:
  --runs-dir train/runs/<latest>/
  --out      train/aggregates/

Output: one JSON file per domain in --out, each conforming to the
DomainPrefit schema in src/prefit.rs.
"""
import argparse
import collections
import json
import statistics
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]


def latest_runs_dir() -> Path | None:
    runs = REPO / "train" / "runs"
    if not runs.exists():
        return None
    candidates = sorted([p for p in runs.iterdir() if p.is_dir()])
    return candidates[-1] if candidates else None


def aggregate_domain(domain_dir: Path) -> dict:
    """Walk a domain's event JSONL files and produce a DomainPrefit dict."""
    blocked_hosts = collections.Counter()
    settle_ms_per_nav = []  # collect policy_trace.elapsed_ms per successful nav

    for events_file in domain_dir.glob("*.events.jsonl"):
        nav_succeeded = False
        nav_elapsed = None
        for line in events_file.read_text().splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            kind = ev.get("event")
            data = ev.get("data") or {}
            if kind == "policy_blocked":
                # Legacy event from PR #2; will switch to script_decision
                # filtered to action=skip in v1.
                host = (data.get("matched") or "").lower()
                if host:
                    blocked_hosts[host] += 1
            elif kind == "policy_trace":
                nav_elapsed = data.get("elapsed_ms")
                # Heuristic: if scripts.executed > 0, treat the nav as
                # producing useful output. Real outcome attribution will
                # come via report_outcome events when the harness emits them.
                scripts = data.get("scripts") or {}
                if scripts.get("executed", 0) > 0:
                    nav_succeeded = True
        if nav_succeeded and nav_elapsed is not None:
            settle_ms_per_nav.append(nav_elapsed)

    # Build the DomainPrefit dict. Empty fields where we have no data.
    prefit = {
        "domain": domain_dir.name,
        "framework": None,
        "blocklist_additions": [h for h, _ in blocked_hosts.most_common()],
        "required_patterns": [],
        "settle_distribution": None,
        "shape_hint": None,
    }
    if settle_ms_per_nav:
        sorted_settle = sorted(settle_ms_per_nav)
        n = len(sorted_settle)
        prefit["settle_distribution"] = {
            "p50_ms": int(sorted_settle[n // 2]),
            "p90_ms": int(sorted_settle[min(n - 1, int(n * 0.9))]),
            "p95_ms": int(sorted_settle[min(n - 1, int(n * 0.95))]),
        }
    return prefit


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs-dir", default=None,
                    help="T1 runs dir (default: latest under train/runs/)")
    ap.add_argument("--out", default=str(REPO / "train" / "aggregates"))
    args = ap.parse_args()

    runs_dir = Path(args.runs_dir) if args.runs_dir else latest_runs_dir()
    if not runs_dir or not runs_dir.exists():
        sys.exit(f"no runs dir found at {runs_dir}; run train/collect.py first")

    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)

    print(f"runs: {runs_dir}")
    print(f"out:  {out_dir}")
    print()

    domain_dirs = [p for p in runs_dir.iterdir() if p.is_dir()]
    if not domain_dirs:
        sys.exit(f"no domain dirs in {runs_dir}")

    for d in sorted(domain_dirs):
        prefit = aggregate_domain(d)
        out_path = out_dir / f"{d.name}.json"
        out_path.write_text(json.dumps(prefit, indent=2))
        print(f"  {d.name:30s} → {out_path.relative_to(REPO)} "
              f"(blocklist_additions={len(prefit['blocklist_additions'])}, "
              f"settle={'yes' if prefit['settle_distribution'] else 'no'})")

    print()
    print(f"wrote {len(domain_dirs)} aggregates")


if __name__ == "__main__":
    main()
