# unbrowser training pipeline

Offline pipeline that produces the prefit bundle the runtime ships with. Lives outside the binary on purpose — none of this code runs at agent navigate-time.

See `docs/probabilistic-policy.md` §6 Track 2 for the architecture.

## Why this exists

The runtime is mostly an inference engine: it reads a prefit bundle and applies per-(domain, framework, task) decisions on first sight. That bundle has to come from somewhere. This directory is where it comes from.

The split is deliberate. Online learning per user is too slow (cold start dominates, ~500 visits per decision to converge), too sparse (long-tail domains never accumulate enough data), and too brittle (CDN bundles get re-hashed weekly, resetting per-bundle posteriors). Centralized offline training fixes all three: ship a trained prior, refine at the edges.

## Phases

- **T1 — Corpus collection** (this PR). `collect.py` drives the binary against a corpus × task × policy matrix, captures Phase A NDJSON events to JSONL.
- **T2 — Aggregation** (next PR). Reads T1's JSONL, computes per-(domain, framework, task) decision parameters by pairing `script_decision` / `policy_trace` with `outcome_reported`.
- **T3 — Packing + validation** (next PR). MessagePack pack, hold-out validation, ship as `prefit/v{N}.bundle`.

## T1 — corpus collection

```bash
# Build the binary first
cargo build --release

# Default: 10-site corpus, all tasks, all policies, 2 runs per cell
python3 train/collect.py

# Subset to one site for quick smoke test
python3 train/collect.py --only cnbc --runs-per-cell 1

# Custom corpus
python3 train/collect.py --corpus my_sites.txt --runs-dir /tmp/test_runs
```

### What gets collected

For each `(url × task × policy × repeat)` cell:

```
train/runs/{timestamp}/
  manifest.json                          # full collection summary
  {domain}/
    {task}_{policy}_{repeat}.events.jsonl  # all stderr NDJSON events
    {task}_{policy}_{repeat}.result.json   # navigate result + task outcome
```

Default matrix (configurable via flags):

| Axis | Values |
|---|---|
| Tasks | `extract` (auto-strategy), `query_links` (`a[href]` count) |
| Policies | `off` (no flags), `blocklist` (`--policy=blocklist`) |
| Repeats per cell | 2 |

For the 10-site corpus that's 10 × 2 × 2 × 2 = **80 navigations** per full collection. With 8s rate limit per host + ~3-5s per nav, expect ~10–15 minutes wall-clock.

### What T2 will read

`*.events.jsonl` lines (one JSON per line) include:
- `navigation_started` — start of nav
- `script_decision` — per external `<script src>`, action ∈ {skip, queued, fetch_failed}
- `script_executed` — per evaled script, with `duration_us` + `error`
- `policy_trace` — per-navigation summary
- `outcome_reported` — bound to `navigation_id` from the driver

These are the building blocks for credit assignment in T2.

### Politeness

- `HOST_RATE_LIMIT_SEC = 8.0` — minimum 8s between navigates to the same host
- Per-RPC budget: 45s (`UNBROWSER_TIMEOUT_MS`) — wide enough for slow SPAs
- Bot-challenged sites (e.g. zillow without cookies) return early with `challenge: {...}` in the result; we record that and move on, no retries
- No headless-Chrome escalation in T1 — that's the runtime's job, not the trainer's

### Output is gitignored

`runs/` is in `.gitignore`. Each collection run can produce hundreds of MB of NDJSON + may carry per-site captured fragments. Aggregated outputs from T2 (and the final prefit bundle) live in separate directories with their own retention policy.

## Out of scope for v1

- Click / form / visual task classes — need site-specific scripts
- Failure replay (toggle one decision, re-run, see if outcome flips) — T2 design choice
- Distributed collection — single machine for now; the corpus is small enough
- Writeback to a central training corpus — drivers' opt-in contribution path is U2, post-v1
