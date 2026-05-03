#!/usr/bin/env python3
"""T3 — pack per-domain aggregates into the runtime prefit bundle.

Reads `train/aggregates/{domain}.json` files (produced by T2) and
optional framework_priors, writes `prefit/v1.bundle.json` — the file
the runtime loads via `include_bytes!` in src/prefit.rs.

For v0: the runtime expects JSON, not MessagePack (spec §6.5 calls for
MessagePack, deferred). Schema in src/prefit.rs's PrefitBundle.

Validation: re-reads the written bundle to confirm round-trip parses,
and warns if any aggregate has no settle_distribution or blocklist_additions
(which would mean T2 produced a degenerate row).

Usage:
  python3 train/pack.py [--in DIR] [--out FILE] [--corpus-size N]
                        [--training-pipeline-version STR]

Defaults:
  --in   train/aggregates/
  --out  prefit/v1.bundle.json
"""
import argparse
import json
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]


# Hand-curated framework_priors used as fallback when the domain isn't
# in the bundle but its framework can be detected at runtime. Keeping
# these in pack.py rather than aggregating them from T1 events for v0 —
# real per-framework aggregation is a follow-up.
FRAMEWORK_PRIORS = {
    "react-18": {
        "framework": "react-18",
        "blocklist_additions": [],
        "settle_distribution": {"p50_ms": 1500, "p90_ms": 3000, "p95_ms": 5000},
    },
    "next-14": {
        "framework": "next-14",
        "blocklist_additions": [],
        "settle_distribution": {"p50_ms": 1200, "p90_ms": 2500, "p95_ms": 4000},
    },
    "static_ssr": {
        "framework": "static_ssr",
        "blocklist_additions": [],
        "settle_distribution": {"p50_ms": 100, "p90_ms": 400, "p95_ms": 800},
    },
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--in", dest="in_dir", default=str(REPO / "train" / "aggregates"))
    ap.add_argument("--out", default=str(REPO / "prefit" / "v1.bundle.json"))
    ap.add_argument("--corpus-size", type=int, default=10)
    ap.add_argument("--training-pipeline-version", default="v0-from-aggregates")
    args = ap.parse_args()

    in_dir = Path(args.in_dir)
    out_path = Path(args.out)

    if not in_dir.exists():
        sys.exit(f"input dir does not exist: {in_dir}\n"
                 f"run train/aggregate.py first")

    domains = {}
    for f in sorted(in_dir.glob("*.json")):
        try:
            d = json.loads(f.read_text())
        except json.JSONDecodeError as e:
            print(f"WARN: skipping malformed {f.name}: {e}", file=sys.stderr)
            continue
        if "domain" not in d:
            print(f"WARN: skipping {f.name}: missing 'domain' field", file=sys.stderr)
            continue
        domains[d["domain"]] = d
        if not d.get("settle_distribution") and not d.get("blocklist_additions"):
            print(f"WARN: {d['domain']} has empty settle + empty blocklist additions — degenerate row",
                  file=sys.stderr)

    bundle = {
        "schema_version": 1,
        "fit_timestamp": int(time.time()),
        "fit_corpus_size": args.corpus_size,
        "training_pipeline_version": args.training_pipeline_version,
        "domains": domains,
        "framework_priors": FRAMEWORK_PRIORS,
    }

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(bundle, indent=2))
    print(f"wrote {out_path.relative_to(REPO)} "
          f"({len(domains)} domains, {len(FRAMEWORK_PRIORS)} framework priors)")

    # Validate: round-trip parse so we catch schema drift early.
    try:
        roundtrip = json.loads(out_path.read_text())
        assert roundtrip["schema_version"] == 1
        assert isinstance(roundtrip["domains"], dict)
        for d, p in roundtrip["domains"].items():
            assert "domain" in p, f"{d}: missing 'domain' field"
            assert isinstance(p.get("blocklist_additions", []), list)
        print("validate: ok")
    except (AssertionError, KeyError) as e:
        sys.exit(f"validate FAILED: {e}")


if __name__ == "__main__":
    main()
