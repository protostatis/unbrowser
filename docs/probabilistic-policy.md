# Probabilistic Execution Policy for unbrowser

**Status:** Implementation spec draft (v2 — prefit-first reframe)
**Author:** unbrowser core
**Last updated:** 2026-05-02

## TL;DR

unbrowser embeds QuickJS, an interpreter ~20–50× slower than V8 on JIT-bound code. The architectural answer most projects reach for is "swap to V8 / `deno_core`" — but that breaks the single-binary deploy story, the cross-compile story, and the auditability story that justify QuickJS in the first place.

We propose a different path: close most of the practical gap with a **probabilistic execution policy layer** that decides what to run, what to skip, what to cache, and how long to wait. The same problem JITs solve (online specialization under uncertainty), framed at the script and API level instead of the bytecode level.

**Critical product constraint: this works on the *first* navigation, not after 500 visits.** A user whose first navigate to cnbc.com is slow won't run a second. The system therefore ships with a **prefit prior bundle** — per-domain decision parameters trained offline against a curated corpus of the popular long head — so the runtime starts informed, not flat. Top ~10K domains are the training target (covers ~80% of agent navigations by traffic). Per-instance learning is a *refinement* on top of the prefit, not a bootstrap from zero.

The headline claim: a real Bayesian policy layer is *available to us specifically* because (a) we don't operate under the constraint that defines production JIT design — single-digit-nanosecond decisions on the hot path — and (b) we ship the prior with the binary instead of asking each user to learn it.

Target outcome: at least 50% median wall-clock improvement versus current `main` on a pinned benchmark corpus **on the first navigate to a prefit-covered domain**, with no more than 1% extraction-quality regression and no new stealth detections. The aspirational ceiling is 70-90% of the practical QuickJS-vs-V8 wall-clock gap on pages where most work is avoidable framework, telemetry, animation, or observer churn.

---

## 1. Motivation

### 1.1 The QuickJS gap, sized honestly

V8's perf advantage over QuickJS is real but most-people-state-it-wrong. The "20–50×" figure is a microbenchmark number on JIT-favorable hot loops. On real-world page loads, the gap is typically 3–10× wall-clock, and a profiled breakdown of where those cycles go looks roughly:

| Category | % of cycles | JIT-recoverable | Work the agent needs |
|---|---|---|---|
| Bundle parse | 5–10% | partial | sometimes |
| Framework init + hydration | 30–50% | yes | usually |
| rAF / transitions / animations | 10–25% | yes | **never** |
| Observers (Intersection/Mutation/Resize) | 5–15% | yes | rarely |
| Analytics / telemetry / ads | 10–30% | yes | **never** |
| Page-specific code agent cares about | 5–20% | yes | yes |

The categories the agent never needs (rAF, telemetry) are often the *majority* of JS execution time. The right perf engineering target is therefore not "make QuickJS faster" but "skip the work the agent doesn't need," with the gap on remaining work absorbed by caching and adaptive deadlines.

### 1.2 Why we can't just swap engines

Switching to `deno_core` / V8 trades the gap for several worse problems:

- Binary size grows from ~5 MB to ~50 MB (V8 snapshot baked in)
- Cross-compile becomes a per-triple ordeal (V8 prebuilts, libc/CRT alignment)
- The "one syscall to deploy" framing in `CLAUDE.md` no longer holds
- Stealth shims that work in QuickJS (looser semantics) often need rewriting in V8 (stricter `defineProperty`)
- The engine becomes un-auditable — V8 is ~2 MLOC of C++ vs QuickJS at ~50 KLOC of C

The architectural commitment to QuickJS is load-bearing for the product's positioning. We're not going to revisit it. Closing the perf gap has to happen *around* QuickJS, not by replacing it.

### 1.3 Why a probabilistic layer is the right shape

Most of the recoverable perf is in decisions:

- **Which scripts to execute** (most ad/analytics scripts contribute nothing to the agent's task)
- **Which DOM APIs to stub or no-op** (rAF, IntersectionObserver, MutationObserver — rarely needed for extraction)
- **When to declare the page settled** (most agent tasks succeed on partial hydration)
- **What to cache as bytecode across sessions** (same React/Next bundles loaded thousands of times)
- **What to preload / speculate** (predictable navigation patterns)

Each of these is a decision under uncertainty with cheap evidence. Perfect setting for a probabilistic policy.

### 1.4 First navigate is the adoption gate

The original draft of this design assumed each user's instance would learn its own per-site posteriors over time. That model has a fatal product flaw: **cold start dominates.**

- Most agent workloads touch the long tail (different sites every navigate)
- Per-site Bayesian learning needs ~hundreds of trials per `(domain × decision)` to converge
- A user whose first navigate is slow churns before the system ever learns anything
- Even for users with temporal locality, the learning happens too slowly relative to product expectations

The fix is to **ship the prior, don't learn it**. A centralized offline pipeline runs the binary against a curated corpus of the popular long head (top ~10K domains by traffic, which covers ~80% of agent navigations). It collects Phase A observability events at scale, aggregates them into per-`(domain, framework, task)` decision parameters, and packs the result into a **prefit bundle** that ships with releases (or downloads on first run).

At runtime, the binary is mostly an **inference engine**:

1. Look up `(domain)` in prefit → if hit, decisions are instant
2. If miss, fall back to `(framework_signature)` coarse prior in the prefit
3. If still miss, structural prior (Tier-1 blocklist + framework defaults)
4. Optional per-user overlay store for users with consistent workloads on uncovered domains

This changes the Bayesian story from "learn slowly per user" to "ship a trained model, refine at the edges." Per-user state stays small. Cold start is rare. Privacy is easy (the prefit ships as static data; user observations don't leave the machine unless they opt in to contribute back to training).

The white paper from §6 onward used to read as "online learning per user." That was wrong. The architecture is **offline trained prior + thin runtime inference**, and the implementation phases below are restructured to reflect that.

---

## 2. Background

### 2.1 JIT as crude empirical Bayes

A modern JIT is a frequentist that doesn't know it's doing inference:

- **Inline caches** record observed shapes per call site. Point estimates only — no probabilities, no confidences.
- **Compile thresholds** are hard counters ("hot after N invocations"). No estimation of whether compilation pays off for *this* function.
- **Each call site learns alone.** Type information at site A is not propagated to site B even when they read the same variable.
- **No prior across runs.** The thousand previous loads of the same React bundle teach the engine nothing carried forward.
- **Binary commit.** Specialized or not — no graceful "70% int, 30% float, generate biased code that's fast for ints and slightly slower for floats."

This works because the underlying distributions are extremely peaked (most call sites are monomorphic), so even crude estimation captures most of the win. But "captures most of the win" is exactly the gap a real probabilistic approach exists to close.

### 2.2 The hot-path budget constraint

The reason production JITs aren't already Bayesian is that **the inference cannot cost more than the speedup it produces.** A property access has a budget of single-digit nanoseconds. You cannot run a Bayesian update in that budget. Whatever data structure you carry has to be readable in two or three memory ops.

So in V8, "real" Bayesian methods don't fit — and the engine has been engineered around the constraint for fifteen years.

### 2.3 We don't have that constraint

unbrowser's policy decisions don't run on a nanosecond budget:

- Script execution decisions happen at parse time (microseconds available)
- API stub decisions happen on first call to the API (microseconds available)
- Settle decisions happen on a per-tick poll (milliseconds available)
- Cache decisions happen between RPC calls (effectively unbounded)

This is the gap in production JIT design we can exploit. We get to build the engine V8 can't afford to build, because we're operating at script-and-API granularity, not bytecode granularity.

---

## 3. Architecture

### 3.1 Three-tier architecture

```
                  OFFLINE (centralized, runs weekly/monthly)
┌──────────────────────────────────────────────────────────┐
│  Training pipeline                                       │
│  - drives binary against curated corpus (top ~10K        │
│    domains × task classes × policy toggles)              │
│  - aggregates Phase A NDJSON events                      │
│  - fits per-(domain, framework, task) decision params    │
│  - packs into prefit.bundle (read-only data file)        │
└──────────────────────────────────────────────────────────┘
                           │
                           │  prefit.bundle (~10–50 MB)
                           ▼  (shipped with release or downloaded once)

                  RUNTIME (the binary)
┌──────────────────────────────────────────────────────────┐
│  Inference layer                                         │
│  (μs decisions: lookup + fallback chain)                 │
│                                                          │
│  per-navigation:                                         │
│  1. lookup prefit[domain]               → if hit, apply  │
│  2. else lookup prefit[framework_sig]   → coarse prior   │
│  3. else structural prior (Tier-1 blocklist + defaults)  │
│  4. optional: per-user overlay (~/.unbrowser/refine.db)  │
└──────────────────────────────────────────────────────────┘
                           │
                           │ policy decisions
                           ▼
┌──────────────────────────────────────────────────────────┐
│  Frequentist Hot Path                                    │
│  (nanosecond decisions, fixed-size state)                │
│  - QuickJS interpreter                                   │
│  - shim invocation                                       │
│  - sufficient-statistic collection                       │
└──────────────────────────────────────────────────────────┘
                           │
                           │ profile data (Phase A events)
                           ▼
              [profile event stream → stderr NDJSON]
                           │
                           ├──► local: optional per-user overlay update
                           └──► opt-in: contribute back to training corpus
```

The runtime is mostly an **inference engine** doing prefit lookups + fallback. There is no large per-user database; per-user state is a small overlay (typically <100 KB) that refines prefit decisions for sites the user hits frequently. All the heavy lifting (corpus collection, aggregation, parameter fitting) is offline and centralized. Updates ship as new prefit bundles on a regular cadence.

The hot path does not change semantically. It collects sufficient statistics (counts, hashes) into fixed-size structures. The inference layer reads those statistics for the current navigation and makes decisions; the prefit holds the prior knowledge that informs those decisions on first sight.

### 3.2 Five decision points

| Decision | Granularity | Budget | Evidence |
|---|---|---|---|
| Skip script | Per `<script>` tag | μs | URL host, content hash, prior site profile |
| Stub API | Per first-call | μs | Per-domain prior on whether this API matters |
| Settle deadline | Per page load | ms | Per-domain time-to-quiescence distribution |
| Bytecode cache hit/miss | Per script | μs | URL + ETag + content hash |
| Speculative preload | Per navigation | ms | Per-domain link-graph priors |

These decisions are intentionally cheap, but they are not statistically independent. Skipping a script changes API-call counts and settle timing; observer stubs change DOM mutation patterns; cached bytecode changes latency without changing page semantics. The implementation therefore records every active policy decision in a per-navigation `policy_trace` and only applies posterior updates that have an attributable outcome. The first version uses independent conjugate posteriors as storage, not as a claim that the page-load system is independent.

---

## 4. Probabilistic model

### 4.1 Notation

For each policy key we maintain conjugate posteriors over the binary outcomes that drive policy decisions. We use Beta-Binomial because:

- Binary outcomes for script and API decisions (needed / not needed)
- Cheap closed-form posterior update (`α += weight` or `β += weight`)
- Natural credible intervals
- Tiny state per posterior (two `f64`s)

Where outcomes are categorical (e.g., type at a polymorphic site), we extend to Dirichlet-Multinomial.

### 4.2 Script-needed posterior

For each `(domain, script_url_pattern)`:

- Random variable: `script_needed = true` means skipping this script can change the driver's task outcome or extracted data.
- Prior: `Beta(α0, β0)` over `P(script_needed)`; informative for known framework, ad, analytics, challenge, and telemetry patterns; uninformative for unknown.
- Evidence: paired or replayed navigations where this script's action changed while the task, URL, cookies, viewport, and policy seed stayed fixed.
- Posterior: standard Beta-Binomial update on attributable outcomes.
- Decision: skip script only if the **upper** credible bound of `P(script_needed)` is below `θ_skip` (for example, `q95 < 0.1`). Using the lower bound is unsafe: `Beta(1, 1)` has a low 5% quantile and would incorrectly skip unknown scripts.

The hard part is the credit-assignment problem — *was* the script's effect needed? We bootstrap with structural priors:

- Hostname matches known ad/analytics blocklist → strong prior toward not-needed
- Script content matches known framework signature (React DOMServer, Vue) → strong prior toward needed
- Inline script with DOM-touching patterns → moderate prior toward needed
- Known bot, challenge, or fingerprinting host → strong prior toward needed unless an explicit stealth profile says otherwise

Updates happen only through the outcome protocol in Section 4.5. A normal successful run with a skipped script is weak evidence that the script was not needed. A failed run is not enough to update a specific script by itself; the implementation must run a targeted replay or bisect before assigning `script_needed = true`.

### 4.3 Site-settled posterior

For each `domain`:

- Maintain empirical distribution of time-to-DOM-quiescence
- Model as `LogNormal` or `Gamma` (skewed, positive)
- Prior: weakly informative based on framework detected
- Evidence: actual settle times observed
- Decision: settle deadline = `quantile(0.9)` of posterior predictive

Concrete: instead of a fixed 3-second timeout, we wait `min(0.9-quantile of learned distribution, hard ceiling)`. For HN this converges to ~50ms; for CNBC it converges to ~2.5s; for a YouTube SPA it converges to ~1.2s after first paint.

### 4.4 API-needed posteriors

For each `(domain, API)` where API ∈ `{rAF, IntersectionObserver, MutationObserver, ResizeObserver, ...}`:

- Random variable: `full_api_needed = true` means the stubbed implementation can change the driver's task outcome or extracted data.
- Prior: biased toward `false` for non-rendering extraction (`Beta(1, 9)` is the starting point), with domain/framework overrides.
- Update: if a stubbed run fails and a controlled replay with only this API promoted to full semantics succeeds, update `full_api_needed = true`; if repeated stubbed runs succeed on the task class, update `false`.
- Decision: install the stub if `P(full_api_needed)` has an upper credible bound below the configured stub-safe threshold (`q95 < θ_stub_safe`, default `0.3`); promote to full implementation when the lower credible bound crosses the configured needed threshold (`q05 > θ_full_needed`, default `0.5`) or when the driver forces promotion.

The stored key must include `(domain, api_name, task_class, framework_signature)` rather than only `(domain, api_name)`. A search-results query and a visual interaction task on the same domain can need different API behavior.

### 4.5 Outcome protocol and credit assignment

The policy layer cannot infer correctness from page load events alone. The driver must report task outcomes explicitly:

```json
{
  "method": "report_outcome",
  "params": {
    "navigation_id": "uuid",
    "task_id": "uuid",
    "task_class": "extract|query|click|form|visual",
    "success": true,
    "quality": 1.0,
    "required_selectors": ["optional css or element refs"],
    "error": null
  }
}
```

`navigation_id` binds the outcome to the `policy_trace` emitted during navigation. The trace records scripts skipped/run, API stubs installed/promoted, cache hits, settle deadline, relevant hashes, and a deterministic policy seed. Posterior updates are allowed only from these evidence classes:

1. **Positive weak evidence:** a task succeeds with a policy decision active. This can increment the "not needed" side with a small weight, for example `0.1`, because success may be unrelated to the decision.
2. **Controlled replay evidence:** the task changes from fail to success, or extracted data changes materially, when exactly one decision is toggled. This is strong evidence and receives weight `1.0`.
3. **Bisect evidence:** when several decisions were active, replay uses binary search over skipped scripts or promoted APIs. Only the isolated culprit receives strong evidence.
4. **No update:** a task fails under many active decisions and no replay is run. Recording the failure is useful for debugging, but it must not corrupt individual posteriors.

This makes the implementation slower when learning from failures, but failure replay is off the hot path and can be sampled. The default sampling policy is: replay all failures in local benchmark mode, replay at most 1% of successful production navigations, and never replay when the driver marks the task as side-effectful.

### 4.6 Bytecode cache value

For each compiled script artifact:

- Track invocation count and last-use timestamp
- Predict P(reused in next N requests) via simple recency-frequency model (or full Bayesian renewal process if we want)
- Evict by lowest expected future value
- This subsumes LRU as a special case (when we have no domain priors)

### 4.7 Why conjugate priors

We deliberately stay in the conjugate-prior corner. No MCMC, no variational inference, no neural nets. Reasons:

- Updates are O(1) memory and cycles
- Posteriors are interpretable (we can inspect them and reason about decisions)
- Failure modes are easy to debug (extreme `α`/`β` values are visible)
- No model artifacts to ship — just a parameter table

If a future version benefits from richer models (neural type prediction across sites, RL for compile scheduling), we add them. v1 is intentionally simple.

---

## 5. Decision policies

### 5.1 The general form

Every policy decision is decision-theoretic:

```
action* = argmax_a  E[utility(a) | posterior]
       =  argmax_a  Σ_outcome  P(outcome | posterior) · u(a, outcome)
```

For binary decisions (skip / run) this collapses to a threshold on the posterior:

```
skip if  saved_runtime_ms * P(not_needed) > regression_cost_ms * P(needed)
```

For v1, the implementation should not expose arbitrary utility functions in the hot path. Each policy uses fixed thresholds tuned offline on the deterministic replay corpus, with a conservative default that preserves baseline behavior on unknown domains.

### 5.2 The settle decision (worked example)

The current Phase 5 plan proposes a settle detector. The probabilistic version:

```python
# Conceptually — actual impl in src/policy/settle.rs
def should_stop_waiting(domain, elapsed_ms, prior_posterior):
    # P(no further meaningful changes | current trajectory)
    p_quiet = prior_posterior.cdf(elapsed_ms)
    # E[task value if we stop now]
    e_value_stop = expected_extraction_quality(elapsed_ms)
    # E[task value if we keep waiting]
    e_value_wait = expected_extraction_quality_after_more_ms(elapsed_ms + WAIT_STEP)
    # Cost of waiting (latency, cpu)
    e_cost_wait = WAIT_STEP * cost_per_ms

    return e_value_stop > e_value_wait - e_cost_wait
```

This converges to "stop early on simple sites, wait longer on heavy SPAs," learned per-domain rather than hardcoded.

### 5.3 The script-skip decision

```python
def should_skip_script(domain, script_url, content_features, prior_store):
    posterior = prior_store.get_or_default(domain, content_features)
    # Upper bound of credible interval for P(script_needed)
    upper_ci = posterior.quantile(0.95)
    # Skip only if we're confident it's not needed
    return upper_ci < SKIP_THRESHOLD  # e.g., 0.1
```

The use of the upper CI bound (rather than mean or lower bound) is the conservative choice: unknown scripts run by default until the posterior has enough evidence that the script is not needed. This minimizes correctness regression.

---

## 6. Implementation plan

The plan is reorganized around the prefit-first architecture from §1.4 / §3.1. Old phase numbering (A–H) was a per-user-learning sequence and is superseded. The new structure has three tracks: **Runtime**, **Offline training**, and **Optional refinement**.

### Track 1 — Runtime (in the binary)

#### Phase A1 — Runtime observability ✅ shipped (PR #4)

NDJSON event stream is live: `navigation_started`, `script_decision`, `script_executed`, `policy_trace`, `outcome_reported`. All carry `schema_version: 1` and `navigation_id`. `report_outcome` RPC method validates inputs. Pairing invariant: `navigation_started` ↔ `policy_trace` always emit together.

This is the foundation for both **offline training data collection** (drive the binary, scrape stderr) and **optional online refinement** (consume own events, update local overlay).

#### Phase R1 — Prefit loader (~3 days)

Read-only access to the shipped prefit bundle.

- Format: **MessagePack** (binary, fast), indexed by domain, with a header containing schema_version, fit_timestamp, and supported framework signatures
- Location: bundled in binary via `include_bytes!` for v1 (~10 MB acceptable; revisit if it grows past ~30 MB), then move to `~/.unbrowser/prefit/v{N}.bundle` with first-run download
- Loader API:
  ```rust
  pub struct Prefit { /* mmap'd or in-memory */ }
  impl Prefit {
      pub fn lookup(&self, domain: &str) -> Option<&DomainPrefit>;
      pub fn fallback(&self, framework: FrameworkSig) -> &FrameworkPrefit;
      pub fn schema_version(&self) -> u32;
      pub fn fit_timestamp(&self) -> SystemTime;
  }
  ```
- Emits `prefit_lookup` event per navigation: `{navigation_id, domain, hit: bool, fallback_used: "framework|structural", fit_age_days}` so drivers can see whether a navigation was prefit-covered.

**Deliverable:** Binary ships with a v0 prefit (even hand-curated for the 10-site test corpus is fine to start) and the inference layer queries it.

#### Phase R2 — Inference layer / fallback chain (~3 days)

Wire prefit into the existing decision points.

- Script-skip: prefit's `blocklist_additions[domain]` extends the Tier-1 blocklist for that nav. Prefit's `required_patterns[domain]` overrides Tier-1 false positives.
- API decisions: prefit's `api_decisions[domain][api_name]` tells the policy whether to stub, coalesce, or run full. Default is current shim behavior; prefit overrides per-site.
- Settle: prefit's `settle_distribution[domain]` provides `quantile(0.9)` deadline. Default is current 5s ceiling.
- Fallback chain: `(domain) → (framework_signature) → structural`. The framework_signature lookup uses cheap window-global probes (does `window.React` exist? `window.__NEXT_DATA__`? etc.) computed during navigate.

**Deliverable:** When `--prefit=on` (default true once R1+R2 land), policy decisions use prefit values; when off, current behavior preserved.

#### Phase C — Bytecode cache (~5 days, unchanged from previous plan)

Independent of prefit work; can land in parallel.

- Hook into the script loader: before compiling, check cache
- Cache key: `sha256(script_content)` + QuickJS/rquickjs version + target triple + bytecode format version + module-vs-script mode + `sha256(shims.js)`. URL is metadata, not identity.
- Backing store: `~/.unbrowser/bytecode/{prefix}/{hash}.qbc`
- Use QuickJS `JS_WriteObject` / `JS_ReadObject`
- **Only load bytecode produced locally** by this binary/configuration. Never accept remote or prefit-shipped bytecode; the prefit ships as data, not executable artifacts.
- LRU eviction with size cap (default 500 MB)

**Deliverable:** Second navigation to same script content reuses bytecode; measurable parse-time reduction on repeat visits to the same site.

### Track 2 — Offline training (separate pipeline, not in the binary)

This is where the real science happens. Lives in a sibling directory (`unbrowser-train/`) or a separate repo.

#### Phase T1 — Corpus collection harness (~5 days)

Scripted multi-pass collection.

- Inputs: `corpus.txt` (initially the 10-site test corpus, then 1K, then 10K), policy-toggle matrix
- For each `(domain, task_class, policy_config)`: navigate, drive an extraction task, capture all NDJSON events to JSONL
- Budget: ~30 navs per domain (covers baseline + various skip / stub / settle toggles for replay-style strong evidence)
- Output: `corpus_runs/{domain}/{run_id}.jsonl` — one file per run
- Politeness: rate-limit per host, respect robots, stop early on bot challenges (let the auto-escalation router handle it during training too)

**Deliverable:** Run end-to-end against the 10-site corpus, produce structured JSONL output.

#### Phase T2 — Aggregation + parameter fitting (~5 days)

JSONL → prefit parameters.

- Per `(domain, framework, task_class)`: aggregate `script_decision` outcomes paired with `outcome_reported` to compute conjugate posteriors
- Per `(domain, api_name, task_class)`: same for API decisions
- Per `(domain, framework)`: compute settle-time distribution (mean, p50, p90, p95) from `settle` field of `policy_trace` paired with successful outcomes
- Discover blocklist additions: scripts that were systematically present and whose absence didn't reduce extraction quality
- Discover required patterns: scripts whose absence caused systematic failures
- Output: per-domain JSON entries

**Deliverable:** A `.json` per training-corpus domain with fitted decision parameters.

#### Phase T3 — Prefit packing + validation (~3 days)

Aggregated entries → shipped binary format.

- Pack into MessagePack with domain-indexed lookup table
- Validate against a holdout split: predict decisions on held-out sites, measure prediction quality
- Stamp `fit_timestamp`, `schema_version`, `corpus_size`
- Output: `prefit/v1.bundle`

**Deliverable:** A bundle that can be loaded by Phase R1's loader and demonstrably improves first-navigate behavior on holdout sites.

### Track 3 — Optional per-user refinement (post-v1)

Ship Tracks 1 and 2 first. Refinement is meaningful only for users with consistent workloads on uncovered domains; most users won't need it.

#### Phase U1 — Per-user overlay store (~3 days)

- Tiny SQLite at `~/.unbrowser/refinement.db`, off by default, opt-in via `--refine=on`
- Captures the user's own outcome events, fits per-`(domain, decision)` posteriors using the same conjugate updates
- At decision time: if user-overlay has `n_observations > threshold` for a key, blend with prefit (e.g., weighted mean of `Beta(α_prefit, β_prefit) + Beta(α_user, β_user)`)
- Cap state: hard limit on entries (LRU); never exceeds ~5 MB

**Deliverable:** Power users with workload on long-tail domains get personalized improvement; everyone else is unaffected.

#### Phase U2 — Opt-in contribution back to training (~3 days)

- `unbrowser contribute --to=https://...` uploads anonymized event JSONL to the training pipeline
- Strict redaction: URLs hashed (only the domain remains), no body, no cookies, no form data, no selectors
- Disabled by default; clear consent flow

**Deliverable:** Training corpus grows from real-world usage.

### Phase H — (optional) Speculative preload

Unchanged from previous plan. Skip unless Tracks 1+2 leave headroom on the success criteria.

---

## 6.5 Prefit data format

The prefit bundle is a single binary file shipped with the release (or downloaded once on first run). MessagePack for compactness + fast deserialization; a domain-indexed lookup table at the head allows O(1) lookup without deserializing unrelated entries.

```
prefit.bundle (binary, MessagePack)
├─ header
│  ├─ schema_version: u32
│  ├─ fit_timestamp: u64 (unix seconds)
│  ├─ fit_corpus_size: u32 (number of training navigations)
│  ├─ training_pipeline_version: string
│  └─ supported_framework_signatures: [string]
├─ domain_index: HashMap<String, ByteOffset>
├─ framework_priors: HashMap<FrameworkSig, FrameworkPrefit>
└─ domain_entries: [DomainPrefit]
```

A single `DomainPrefit` row, ~1 KB compressed:

```jsonc
{
  "domain": "cnbc.com",
  "framework": "react-18",
  "fit_corpus_size": 30,                  // training nav count for this domain
  "fit_timestamp": 1746230400,
  "blocklist_additions": [
    "zephr-templates.cnbc.com/personalize.js",
    "assets.bounceexchange.com/*"          // glob — supports tail wildcards
  ],
  "required_patterns": [
    "i.cnbc.com/_next/static/chunks/framework-*.js"
  ],
  "api_decisions": {
    "intersection_observer": {"action": "stub", "alpha": 1.5, "beta": 28.0},
    "mutation_observer":     {"action": "full", "alpha": 12.0, "beta": 2.0},
    "request_animation_frame": {"action": "coalesce", "alpha": 1.0, "beta": 30.0}
  },
  "settle_distribution": {
    "p50_ms": 1800, "p90_ms": 2500, "p95_ms": 4000,
    "extraction_succeeds_at_p90_pct": 0.96
  },
  "shape_hint": "spa_shell_with_density"   // see CLAUDE.md BlockMap density
}
```

`FrameworkPrefit` has the same shape but with `domain` replaced by `framework_signature` and aggregated stats over all training domains using that framework. This is the second-tier fallback when a domain isn't in the prefit but its framework is recognized at runtime.

**Versioning:** the binary's prefit loader checks `schema_version` and rejects bundles with a higher version (forward-incompatible) or older than `min_schema_version` (back-incompatible). Releases bump versions only when the runtime-loadable schema changes; routine retraining bumps `fit_timestamp` only and ships under the same `schema_version`.

**Audibility:** the bundle is plain MessagePack, no executable artifacts. `unbrowser prefit dump --domain=cnbc.com` will print the entry as JSON for inspection. The training pipeline keeps the source JSONL events for any shipped bundle, so any decision can be traced back to its training observations.

**Update cadence:** v1 ships hand-curated entries for the 10-site test corpus (covers the demoable case). v2+ ships pipeline-trained bundles refreshed weekly or monthly.

---

## 7. Test plan

### 7.1 Corpus

Use two corpora:

1. **Deterministic replay corpus**: pinned HTML, scripts, headers, cookies, viewport, timezone, and network responses captured with permission or generated from fixtures. This is the acceptance corpus for posterior updates, extraction quality, bytecode cache correctness, and performance deltas.
2. **Live canary corpus**: popular real sites used only for smoke testing bot/challenge behavior, current framework shapes, and drift. Live canary results are reported separately and cannot be the sole evidence for shipping a policy change.

Replay targets should cover the major site shapes from `CLAUDE.md`'s "Quick reference by site type":

| Site | Type | Why |
|---|---|---|
| `news.ycombinator.com` | Static SSR | Baseline — should be near-instant |
| `en.wikipedia.org` | Static SSR | Another simple case |
| `github.com` | React SSR + hydration | Hydration-heavy |
| `npmjs.com` | Next.js SSR + hydration | Different framework |
| `reddit.com` | Web components | Tests host-attrs path |
| `youtube.com` | Data-store SPA | Tests intel_stores path |
| `cnbc.com` | SSR shell + JS-filled cells | The "density: likely_js_filled" trap |
| `polymarket.com` | Card grid SPA | Heavy lazy-load |
| `kalshi.com` | Card grid SPA | Different impl, same shape |
| `zillow.com/homes/for_rent/` | Bot-protected SSR | Tests interaction with cookies/challenges |

Excluded from policy-affected acceptance runs: any site requiring login state, geolocation-sensitive personalization, or non-deterministic account/cookie state. Those belong in the live canary suite.

### 7.2 Metrics

The product framing in §1.4 ("first navigate is the adoption gate") drives the metric priorities — `first_nav_*` rows are the headline acceptance criteria, `repeat_nav_*` rows are nice-to-haves.

| Metric | What it measures | Target |
|---|---|---|
| **first_nav_wall_clock_ms (prefit-covered)** | Time to blockmap on **first** visit to a domain in prefit | **-50% median, -70% p90 vs baseline** |
| **first_nav_extraction_success_pct** | Agent task succeeds on first visit, prefit-covered domain | **≥ 99% of baseline** |
| `first_nav_wall_clock_ms (prefit-miss)` | First visit to a domain *not* in prefit | ≤ 5% regression vs baseline |
| `prefit_hit_rate` | (navigations to prefit-covered domains) / (total navigations) on a representative workload | ≥ 80% on the corpus |
| `repeat_nav_wall_clock_ms` | Wall-clock on 2nd+ visit (with bytecode cache active) | -60% median vs baseline |
| `scripts_executed_count` | Number of `<script>` tags actually run | -50% on commercial sites, ≤baseline elsewhere |
| `scripts_skipped_count` | Number skipped by policy | report only |
| `blockmap_completeness` | # interactives + # headings detected | ≥ 95% of baseline |
| `query_result_accuracy` | For 10 fixed queries per site | ≥ 99% of baseline |
| `bytecode_cache_hit_rate` | Hits / (hits + misses) | ≥ 80% on second+ visit |
| `stealth_signal_count` | Bot-detection probes that fire | ≤ baseline (no regressions) |
| `binary_size_mb` | Stripped release binary (incl. bundled prefit) | ≤ baseline + 30 MB (acceptable cost of first-nav speedup) |

### 7.3 Configurations

Five configurations, paired across all targets. Holding prefit constant tests the runtime; varying prefit tests the training pipeline.

- **C0 (baseline)**: current `main`, no policy, no prefit
- **C1 (observability only)**: Phase A1 enabled, no behavior-changing decisions. Performance-neutral baseline for comparison.
- **C2 (cache only)**: bytecode cache (Phase C) — repeat-visit win, no first-visit win
- **C3 (prefit + cache, no overlay)**: Phases R1+R2+C with shipped prefit. **This is the headline configuration** — first-nav speedup from prefit, repeat-nav from cache.
- **C4 (full incl. refinement)**: C3 + per-user overlay (Phase U1). Tests whether refinement adds anything for users with workload concentration.

Acceptance is on **C3 vs C0** for first-nav metrics; **C2 vs C0** for repeat-nav; **C4 vs C3** as the marginal gain from refinement (expected small).

### 7.4 Methodology

For each `(target, configuration)`:

1. Pin OS, CPU governor, viewport, timezone, user agent, and network replay fixture.
2. Run 100 cold runs (cleared cache, fresh process) to measure startup.
3. Run 100 warm runs (cache populated, process reused) to measure steady state.
4. Record all metrics and the full `policy_trace`.
5. Compute paired differences C0→C1, C0→C2, C0→C3, and C2→C3.
6. 95% bootstrap CIs (10k resamples) on each difference.
7. Report p-values for "no difference" null hypothesis (Wilcoxon signed-rank for non-parametric).

A change is considered an improvement only if the lower CI bound is on the favorable side of zero.

### 7.5 Acceptance criteria

The implementation ships when (all measured on **the first navigate** to each domain in the corpus, fresh process, no warmup):

- **C3 vs C0 on prefit-covered first-nav: median wall-clock improvement ≥ 50%, lower CI bound > 30%** (the headline product metric — the user's first impression)
- **C3 vs C0: extraction success rate change is in `[-1%, +∞)` on prefit-covered domains** (no quality regression for what we promise to handle)
- C3 vs C0 on prefit-miss first-nav: wall-clock regression ≤ 5% (uncovered domains are no worse than baseline)
- C3 vs C0: stealth signal count is in `(-∞, +0)` (no new detections)
- C2 alone (cache only) on repeat-nav: ≥ 30% wall-clock improvement (sanity check on the cache)
- Prefit hit rate ≥ 80% on the test corpus (otherwise the headline metric isn't actually testing the headline)
- C1 vs C0: median wall-clock regression < 5% (observability is cheap enough to leave on)
- All schema-versioned state is migratable across versions

If any criterion fails, ship the components that pass and revisit the failing one. The first two are non-negotiable for the product framing in §1.4 — without them, the prefit-first architecture is unjustified.

### 7.6 Adversarial testing

Beyond corpus performance, four adversarial tests:

1. **Prefit drift**: simulate a site that redeploys its bundle (changes `framework_signature` or breaks a `required_pattern`). Verify the runtime falls back to coarser tier safely; verify the training pipeline detects the drift on the next collection cycle.
2. **Stealth probe**: load fingerprint.com or similar. Verify prefit's `blocklist_additions` don't include fingerprinting probes the page expects.
3. **Cold-start performance (no prefit hit)**: visit a domain not in prefit, with a fresh process. Verify performance is no worse than C0 (structural-prior fallback should default to baseline behavior).
4. **Stale prefit**: artificially advance the system clock by 6 months. Verify the runtime warns about prefit staleness and offers to refresh; verify decisions still default to safe behavior.

---

## 8. Risks

### 8.1 Correctness regression (high)

Aggressive script-skipping or API-stubbing can break extraction in subtle ways. Mitigations:

- Conservative thresholds (use the upper credible bound for "needed" probabilities, not the mean or lower bound)
- Per-domain override mechanism for the agent driver to force "run everything" mode
- Continuous monitoring of `extraction_success_rate` in profile stream
- Rollback via controlled replay or manual override. Failed tasks alone are not assigned to individual policy decisions.

### 8.2 Stealth signal loss (medium)

Some sites probe specifically for the *side effects* of running their fingerprinting script. If we skip it, the absence is itself a signal. Mitigations:

- Maintain a "stealth-required" allowlist that overrides skip decisions
- Add stealth tests to the test corpus (fingerprint detection sites)
- Bias priors toward "run" when host matches a known fingerprinting service (the cost of skipping is high)

### 8.3 Profile poisoning (medium)

Adversarial site changes its bundle, our prior becomes wrong. Mitigations:

- TTL on posteriors (default 30 days)
- Detect concept drift: large shift in observed time-to-settle triggers prior reset
- Bound the influence of any single observation (cap `α + β` at e.g. 1000)

### 8.4 Privacy / multi-tenancy (medium)

Cross-session priors are persisted state. If unbrowser is run as a shared service, one user's profile shouldn't leak to another's. Mitigations:

- Posterior store is in-memory per process by default (no shared state across users)
- Persistent stores require an explicit path (`--prior-store`) and should be namespaced per tenant in shared services
- Optional shared-prior mode requires explicit opt-in and must never include user-specific URLs, selectors, form values, or cookies
- "Starter pack" priors are read-only and shipped as data; no user-specific state

### 8.5 Maintenance / migration (low)

Posterior schema changes break stored state. Mitigations:

- Version the SQLite schema; run migrations on startup
- Posterior store can be rebuilt from profile event log (lossy but recoverable)
- Provide `unbrowser reset-priors` for nuclear option

### 8.6 The decision-theoretic claim is harder than it sounds (low)

The "argmax over expected utility" framing is clean in this doc but each `u(a, outcome)` has to be calibrated against real costs. Wrong utility = wrong decisions even with perfect posteriors. Mitigations:

- Start with simple thresholds (skip if `P(needed) < 0.1`), not full utility computation
- Tune thresholds on the test corpus before the more elaborate machinery
- Keep utility functions visible and configurable, not buried

### 8.7 Bytecode cache safety (medium)

QuickJS bytecode is an internal executable representation, not a stable or safe interchange format. Mitigations:

- Cache only bytecode produced locally by the same binary/configuration
- Include engine version, target triple, compile flags, module mode, and shim hash in the cache key
- Treat the cache directory as trusted local state; use restrictive permissions and never import bytecode from starter packs or remote sources
- Fall back to source compilation on any cache read, version, or validation failure

---

## 9. Out of scope for v1

- **Engine-internal inline cache improvements** (PrimJS / QuickJS-NG IC enhancements). These are engine-level changes that compose with this work. Tackle in a separate PR if/when needed.
- **Cross-user federated learning of priors.** Privacy and operational complexity not justified at current scale.
- **Online RL / neural type prediction.** Conjugate Bayesian is sufficient for v1. Revisit if and when we have data showing simple methods plateau.
- **Speculative preload (Phase H above).** Optional; only if v1 metrics justify additional complexity.
- **Modifying the `rquickjs` crate or QuickJS itself.** Everything in this doc is implementable as a layer on top.

---

## 10. Success criteria

The work is successful if six months after merging:

1. **First-nav perf** (the headline product metric per §1.4): ≥ 50% median wall-clock improvement on the prefit-covered subset of the corpus, on the *first* visit to each domain (no warmup). Target 70%; ceiling ~85% past which we're in JIT-only territory.
2. **First-nav quality**: Extraction success rate within 1% of baseline on prefit-covered domains.
3. **Coverage**: Prefit hit rate ≥ 80% on a representative agent workload.
4. **Stealth**: No new detections on commodity-tier bot-protected sites.
5. **Cold-miss safety**: First-nav on uncovered domains is ≤ 5% worse than baseline (the prefit-miss path doesn't pessimize).
6. **Maintainability**: Total runtime LOC added < 5,000 (Rust + JS). Training-pipeline LOC is separate and unbounded — it's not in the binary.
7. **Engine independence**: Inference layer is engine-agnostic — could swap to PrimJS or future QuickJS-NG without policy changes.
8. **Auditable**: Prefit bundle is `unbrowser prefit dump`'able; any decision can be traced to its training observations via the pipeline's retained JSONL.
9. **Refresh cadence**: Training pipeline runs at least monthly; prefit ships under semver-compatible bumps for routine retraining.

---

## 11. Open questions

These don't block the architecture, but each must be resolved before enabling behavior-changing policy decisions by default:

1. **What is the minimal useful quality schema?** `report_outcome` defines the transport, but each driver still needs to decide whether it can provide only `success`, a numeric `quality`, expected selectors, or richer task-specific assertions.

2. **How are framework signatures detected?** Useful for informative priors. Options: hash-prefix matching, AST-shape signatures, or just "first two lines of the bundle" heuristic. Probably the cheapest thing that works.

3. **What's the posterior store's update model under concurrency?** SQLite writes are serialized but the update path is hot. Should we batch updates? Use WAL mode? Probably yes to both.

4. **Do we ship priors as part of the binary or as a separate data file?** Embedding bloats the binary; external file is one more thing to install. Probably external with a `--with-priors=path` flag.

5. **How much replay is acceptable outside benchmarks?** Cold-start of a new domain is intentionally conservative and runs unknown scripts. Production replay should be sampled and disabled for side-effectful tasks, but the exact budget should be tuned against real usage.

---

## 12. Why this is the right work to do now

Three reasons this is the right priority:

1. **The gap is real and growing.** Modern web bundles get heavier every year. The QuickJS-vs-V8 gap on hot loops doesn't shrink — but the fraction of *necessary* hot loops on any given page is shrinking, because more and more page time is spent on framework cruft and instrumentation that an LLM agent doesn't care about. The probabilistic policy approach captures all of that.

2. **It's architecturally aligned.** unbrowser's elevator pitch is "WebFetch but stateful and interactive" with "LLM-native output (BlockMap + element refs)." The probabilistic policy layer is the same shape of insight applied to *execution*: don't run things the LLM doesn't need. Same product philosophy, applied one layer down.

3. **It scales with the project.** Every new domain unbrowser sees, every new agent task it serves, makes the priors better. The system gets faster the more it's used. That's a property no engine swap delivers.

---

## Appendix A: A worked example

Concrete trace through a `cnbc.com` page load under the proposed system:

1. Driver calls `navigate("https://cnbc.com/markets")`.
2. `rquest` GET returns 626 KB HTML with 47 `<script>` tags.
3. HTML parsed by html5ever, VDOM seeded.
4. Script filter (Phase D) consults priors:
   - 31 scripts match analytics/ad blocklist → skipped (strong structural prior)
   - 8 scripts hash-match cached compiled bytecode → loaded from cache (Phase C)
   - 5 scripts are inline framework code → posterior says run, run them
   - 3 scripts are unknown → uninformative prior, run by default (conservative)
5. API stubs (Phase E) installed:
   - `IntersectionObserver` stubbed (cnbc posterior strongly favors stub-OK)
   - `rAF` coalesced
   - `MutationObserver` stubbed
6. Scripts execute. Stub'd observers fire synthetic events; lazy-loaded content materializes immediately.
7. Settle detector (Phase F) consults posterior: cnbc's posterior says median settle 1.8s, p90 2.5s. Wait until 2.5s or DOM-quiet, whichever first.
8. At 1.6s, DOM-quiet detected. Return blockmap.
9. Agent issues `query("table.markets-table tbody tr")`. `density.likely_js_filled` had been true on baseline; with stubs, table is now populated.
10. Driver reports `report_outcome {navigation_id, task_id, task_class: "query", success: true, quality: 1.0}`.
11. Posteriors updated:
    - `(cnbc.com, IntersectionObserver, query, framework_signature, full_api_needed)` → weak evidence toward `false`
    - `(cnbc.com, settle_time)` → log-time observation added
    - `(cnbc.com, script_X, query, framework_signature, script_needed)` for each skipped script → weak evidence toward `false`
    - no strong evidence is assigned unless a controlled replay toggles one decision and changes the outcome

End-to-end wall-clock estimate (rough, requires validation):
- Baseline (current `main`): ~4.2s
- C3 (full policy): ~1.9s
- Speedup: 2.2× wall-clock

---

## Appendix B: Why not RL?

Reinforcement learning is the obvious "machine learning fix for online decisions." We deliberately don't go there in v1 for three reasons:

- **Sample inefficiency.** RL needs many trajectories per state-action pair. Our action space is large (skip/run per script, stub/full per API, deadline per ms-bucket), our trajectories are short (one page load), and our reward signal is sparse (one extraction success per task). Bayesian methods with informative priors are far more sample-efficient at this scale.
- **Interpretability.** A posterior with two parameters per decision is fully inspectable. An RL policy is not.
- **Reward design risk.** An RL agent will exploit any miscalibration in the reward function. With Bayesian + threshold, the failure modes are visible (extreme α/β values) rather than hidden in policy weights.

The doors aren't closed — if v1 plateaus and we have data showing Bayesian is the bottleneck, RL on top of the same profile stream is a natural extension.

---

## Appendix C: Connection to the broader product

This work is a specific instance of a more general claim about LLM-driven systems:

> Most performance engineering today uses hand-tuned heuristics where a real probabilistic policy would do better. The constraint that justified hand-tuning (microsecond budgets, no idle time) doesn't bind in LLM-driven workflows because the LLM itself is the slow part. Everything below the LLM has spare budget for inference that previous-generation systems couldn't afford.

unbrowser is one place to demonstrate this pattern. If it works here, the same shape — frequentist hot path, Bayesian policy layer, persistent priors per workload class — applies to many systems-level decisions in LLM-adjacent infrastructure.
