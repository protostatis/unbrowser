# unbrowser training pipeline

Offline pipeline that produces the prefit bundle the runtime ships with. Lives outside the binary on purpose — none of this code runs at agent navigate-time.

See `docs/probabilistic-policy.md` §6 Track 2 for the architecture.

## Why this exists

The runtime is mostly an inference engine: it reads a prefit bundle and applies per-(domain, framework, task) decisions on first sight. That bundle has to come from somewhere. This directory is where it comes from.

The split is deliberate. Online learning per user is too slow (cold start dominates, ~500 visits per decision to converge), too sparse (long-tail domains never accumulate enough data), and too brittle (CDN bundles get re-hashed weekly, resetting per-bundle posteriors). Centralized offline training fixes all three: ship a trained prior, refine at the edges.

## Pipeline order

```
T1 collect  →  T2 aggregate  →  T3 pack
collect.py     aggregate.py     pack.py
   │              │                │
   ▼              ▼                ▼
runs/         aggregates/      *.bundle
```

- **T1 — Corpus collection** (`collect.py`). Drives the binary against a corpus in parallel, captures Phase A NDJSON events to JSONL plus a per-site outcome.
- **T2 — Aggregation** (`aggregate.py`). Reads T1's JSONL, computes per-(domain, framework, task) decision parameters by pairing `script_decision` / `policy_trace` with `outcome_reported`.
- **T3 — Packing + validation** (`pack.py`). MessagePack pack, hold-out validation, ship as `prefit/v{N}.bundle`.

## T1 — corpus collection

```bash
# Build the binary first
cargo build --release

# Quick smoke (3 sites, parallel — used in CI for harness regression)
python3 train/collect.py --smoke 5

# Full 100-site corpus, 16-way parallel (~10–30 min wall-clock)
python3 train/collect.py --concurrency 16

# Custom corpus path
python3 train/collect.py --corpus my_sites.json

# Subset filter (substring match on URL)
python3 train/collect.py --only cnbc

# Disable defaults
python3 train/collect.py --no-policy            # don't pass --policy=blocklist
python3 train/collect.py --no-exec-scripts      # navigate without page-script execution

# Legacy site×task×policy×repeat matrix mode (PR #5 era; 10-site corpus_v1.txt)
python3 train/collect.py --legacy-matrix
```

`UNBROWSER_BIN` overrides the binary path (defaults to `target/release/unbrowser`).

### Corpus

Default corpus is `train/corpus/seed_sites.json` — 100+ hand-curated URLs spanning ten categories: `static_news`, `news_media`, `marketplace_spa`, `ecommerce`, `developer_tools`, `social_csr`, `documentation`, `media_streaming`, `consumer_apps`, `government_static`. Each entry carries `category`, optional `expected_framework`, and a one-line `notes` field.

A `.txt` corpus (one URL per line, `#` for comments) also works — that's how `corpus_v1.txt` is loaded in `--legacy-matrix` mode.

### Concurrency model

- `concurrent.futures.ThreadPoolExecutor` with `--concurrency` workers (default 8).
- Each worker spawns its own `unbrowser` subprocess, sends one `navigate` RPC, reads the response, sends `report_outcome`, then sends `close`.
- Per-site wall-clock budget: `--timeout-s` (default 60s). On timeout the child is killed.
- One automatic retry on `timeout` or `subprocess_crash`. We do **not** retry on a real navigate error (4xx, parse error, exec error) — that's data, not failure.
- Stderr is streamed to a per-site file at spawn (`stderr=open(events_path, 'w')`). **Do not** switch to `stderr=PIPE` and drain at end — a noisy SPA fills the pipe buffer (~64 KB) before the binary writes its JSON-RPC response, deadlocking the parent on `stdout.read`. (PR #5 review HIGH.)

### Outcome categories

Each site lands in exactly one of:

| Outcome | Meaning |
|---|---|
| `ok` | navigate returned 2xx, scripts ran, blockmap present |
| `non_2xx` | binary returned a result but with status ≥ 400 (or 0/missing) |
| `challenge_blocked` | bot detection challenge surfaced in navigate result |
| `exec_error` | binary's response carried a JSON-RPC `error` field |
| `parse_error` | response wasn't valid JSON-RPC |
| `subprocess_crash` | binary exited unexpectedly before responding |
| `timeout` | per-site wall clock exceeded |
| `other` | classified-but-unknown |

T2 will only consider `ok`, `non_2xx`, `challenge_blocked`, and `exec_error` as "data"; the rest are infra failures and dropped.

### What gets collected

```
train/runs/{timestamp}/
  manifest.json                    # parameters used for this run
  _summary.json                    # aggregate report (counts, per-site summaries)
  {domain}/
    navigate.events.jsonl          # all stderr NDJSON events for this site
    result.json                    # navigate result + per-site outcome
```

`*.events.jsonl` lines (one JSON per line) include:

- `navigation_started` — start of nav
- `script_decision` — per external `<script src>`, action ∈ {skip, queued, fetch_failed}
- `script_executed` — per evaled script, with `duration_us` + `error`
- `policy_trace` — per-navigation summary
- `outcome_reported` — bound to `navigation_id` from the driver

These are the building blocks for credit assignment in T2.

### Progress

Each completed site emits one line to **stderr**, e.g.:

```
[ 12/100] OK                 https://news.ycombinator.com/  9 events  0.8s
[ 13/100] CHALLENGE_BLOCKED  https://www.zillow.com/...     5 events  4.2s
```

A single machine-readable JSON line is also written to **stdout** at the end:

```json
{"runs_dir": "...", "elapsed_s": 412.3, "outcomes": {"ok": 78, ...},
 "categories": {"static_news": 10, ...}}
```

This makes the collector pipe-friendly in CI without a TTY-only progress bar.

## T2 — aggregation

```bash
python3 train/aggregate.py                       # latest run under train/runs/
python3 train/aggregate.py --runs-dir train/runs/20260503T160000Z/
```

Writes `train/aggregates/{domain}.json` files conforming to the `DomainPrefit` schema in `src/prefit.rs`.

## T3 — pack

```bash
python3 train/pack.py --in train/aggregates/ --out prefit/v1.bundle
```

Bundles aggregated per-domain prefits into a single MessagePack artifact for the runtime to load.

## Tests

```bash
# Run the smoke test (real-network, hits example.com)
python3 -m unittest train.test_collect -v

# Skip in CI when network's locked down
NO_NETWORK_TESTS=1 python3 -m unittest train.test_collect -v
```

## Politeness & ethics

- Spread the corpus across hostnames so no single host gets hit hard. The default 100-site corpus has at most a handful of URLs per host (mostly ≤ 2).
- We don't follow redirects past the first hop; we don't crawl deep; we don't fetch non-script subresources.
- Bot-challenged sites (e.g. zillow without cookies) return early with `challenge: {...}` in the result and land in `challenge_blocked`. We record that and move on — no retries, no escalation.
- Headless-Chrome escalation is the runtime's job, not the trainer's.

## Out of scope for v1

- Click / form / visual task classes — need site-specific scripts. Use `--legacy-matrix` if you need the old multi-task matrix.
- Failure replay (toggle one decision, re-run, see if outcome flips) — T2 design choice.
- Distributed collection — single machine for now.
- Writeback to a central training corpus — drivers' opt-in contribution path is U2, post-v1.

## Output is gitignored

`runs/`, `aggregates/`, and `*.bundle` are in `.gitignore`. Each collection run can produce hundreds of MB of NDJSON; aggregated outputs and the final bundle live under their own retention policies.
