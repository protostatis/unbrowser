# Probabilistic Execution Policy for unbrowser

**Status:** Implementation spec draft
**Author:** unbrowser core
**Last updated:** 2026-05-02

## TL;DR

unbrowser embeds QuickJS, an interpreter ~20–50× slower than V8 on JIT-bound code. The architectural answer most projects reach for is "swap to V8 / `deno_core`" — but that breaks the single-binary deploy story, the cross-compile story, and the auditability story that justify QuickJS in the first place.

We propose a different path: close most of the practical gap with a **probabilistic execution policy layer** that decides what to run, what to skip, what to cache, and how long to wait. The same problem JITs solve (online specialization under uncertainty), framed at the script and API level instead of the bytecode level.

The headline claim: a real Bayesian policy layer is *available to us specifically* because we don't operate under the constraint that defines production JIT design — single-digit-nanosecond decisions on the hot path. We have microsecond-to-millisecond budgets at the policy layer and can afford inference that V8 cannot.

Target outcome: at least 50% median wall-clock improvement versus current `main` on a pinned benchmark corpus, with no more than 1% extraction-quality regression and no new stealth detections. The aspirational ceiling is 70-90% of the practical QuickJS-vs-V8 wall-clock gap on pages where most work is avoidable framework, telemetry, animation, or observer churn.

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

### 3.1 Two-layer design

```
┌─────────────────────────────────────────────────┐
│  Bayesian Policy Layer                          │
│  (microsecond-to-millisecond decisions)         │
│  - script filter                                │
│  - API stub activation                          │
│  - settle deadline                              │
│  - bytecode cache eviction                      │
│  - speculative preload                          │
└─────────────────────────────────────────────────┘
                      │
                      │ policy decisions
                      ▼
┌─────────────────────────────────────────────────┐
│  Frequentist Hot Path                           │
│  (nanosecond decisions, fixed-size state)       │
│  - QuickJS interpreter                          │
│  - shim invocation                              │
│  - sufficient-statistic collection              │
└─────────────────────────────────────────────────┘
                      │
                      │ profile data
                      ▼
              [profile event stream]
                      │
                      ▼
┌─────────────────────────────────────────────────┐
│  Posterior Store                                │
│  (per-domain, per-framework priors)             │
│  - sqlite or jsonl                              │
│  - conjugate posteriors, cheap online updates   │
└─────────────────────────────────────────────────┘
```

The hot path does not change semantically. It collects sufficient statistics (counts, hashes) into fixed-size structures. The policy layer reads those statistics and makes decisions. The posterior store carries learning across sessions.

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

### Phase A — Profile collection infrastructure (1–2 weeks)

**Goal:** Make the system observable enough to learn from without changing runtime behavior.

- Extend NDJSON event stream on stderr to emit:
  - `navigation_started {navigation_id, url, domain, task_id?, task_class?, policy_seed}`
  - `script_loaded {navigation_id, script_id, url, host, size, content_hash, features}`
  - `script_decision {navigation_id, script_id, action: "run|skip", reason, posterior?, structural_prior}`
  - `script_executed {navigation_id, script_id, duration_us, error?}`
  - `api_policy {navigation_id, api_name, action: "stub|full|promoted", reason, posterior?}`
  - `api_called {navigation_id, api_name, count}` (for rAF, observers)
  - `settle_observed {navigation_id, ms_since_navigate, dom_mutations_since_last}`
  - `policy_trace {navigation_id, decisions, cache_hits, settle_deadline_ms}`
- Add `report_outcome {navigation_id, task_id, task_class, success, quality?, required_selectors?, error?}` to the driver-facing API.
- Add an optional `--profile-log path` flag for a `tail -f`-friendly JSONL file. Default behavior emits NDJSON only for the current process and does not persist user browsing profiles.
- Every event is schema-versioned with `schema_version`.

**Deliverable:** Run unbrowser against the test corpus, get readable profile data, and bind each driver outcome to exactly one navigation trace.

### Phase B — Posterior store (1 week)

**Goal:** Persist and query priors per policy key.

- Store location: process-local in-memory by default; persistent SQLite only when `--prior-store path` is supplied. A convenience default such as `~/.unbrowser/priors.db` is acceptable for the CLI, but shared-service callers must opt in explicitly.
- Schema: `posteriors(scope, key, alpha, beta, n_observations, last_updated, schema_version)`
- Online update API: `prior_store.update(scope, key, outcome: bool, weight: f64, evidence_source)`
- Query API: `prior_store.posterior(domain, key) -> Beta(α, β)`
- Keys include the policy dimension: script keys include `(domain, script_url_pattern, content_hash_prefix, task_class, framework_signature)`; API keys include `(domain, api_name, task_class, framework_signature)`.
- TTL on posteriors: priors decay if `last_updated > 30 days` (concept drift defense)
- Cap effective sample size (`alpha + beta <= 1000`) so old evidence can be overcome.
- Use WAL mode and batched writes; the hot path records observations to an in-memory queue and the policy updater owns SQLite writes.

**Deliverable:** Round-trip `update → query` works in memory; when `--prior-store` is set, the posterior also survives process restart.

### Phase C — Bytecode cache (1 week)

**Goal:** Stop re-parsing the same scripts.

- Hook into the script loader: before compiling, check cache
- Cache key includes `sha256(script_content)`, QuickJS/rquickjs version, target triple, bytecode format version, module-vs-script mode, compile flags, and `sha256(shims.js)` when shims can affect compilation context. URL is metadata, not identity.
- Backing store: `~/.unbrowser/bytecode/{prefix}/{hash}.qbc`
- Use QuickJS `JS_WriteObject` / `JS_ReadObject`
- Only load bytecode produced locally by this binary/configuration. Never accept remote or starter-pack bytecode; shipped priors are data, not executable artifacts.
- LRU eviction with size cap (default 500 MB)
- Future: replace LRU with expected-future-value eviction (Phase G)

**Deliverable:** Second navigation to same domain reuses bytecode; measurable parse-time reduction.

### Phase D — Script filter (1–2 weeks)

**Goal:** Skip scripts that don't matter.

- Bootstrap with structural priors:
  - Hostname matched against EasyList → strong skip prior
  - Hostname matched against framework CDN allowlist → strong run prior
  - Default → uninformative prior, run by default
- Hook between HTML parse and script execution
- Emit `script_skipped` events for visibility
- Skip decision uses the upper credible bound for `P(script_needed)` (conservative)
- Unknown scripts run by default unless a structural prior is strong enough to make the upper bound safe.
- All skip decisions are reversible by per-navigation override (`run_all_scripts`) and by targeted failure replay.

**Deliverable:** Measurable script-execution-count reduction on commercial pages with no extraction quality regression.

### Phase E — Adaptive API stubs (1–2 weeks)

**Goal:** Stub expensive observer APIs by default; activate per-domain when needed.

- Default stubs in `shims.js`:
  - `rAF` → coalesced (run callback once, don't re-schedule)
  - `IntersectionObserver` → returns "all entries intersecting" synchronously
  - `MutationObserver` → no-op queue
  - `ResizeObserver` → fires once with viewport dims
- Per-domain override: if posterior says "needed," install full implementation
- Driver hook: agent can force-activate for sites where it matters
- Use the outcome protocol before updating API posteriors; a failed task does not by itself prove a stub was responsible.

**Deliverable:** Pages that lazy-load on scroll suddenly settle in one tick on stub-OK domains; no regressions on stub-NOT-OK domains.

### Phase F — Settle deadline learner (1 week)

**Goal:** Replace fixed timeout with per-domain learned distribution.

- Track time-to-quiescence per `(domain, framework_detected)` tuple
- Posterior predictive over settle time
- New navigate option: `wait_settle: true` uses 0.9-quantile + small buffer
- Hard ceiling override (default 5s, configurable)
- Outcome updates can shift the deadline later when successful extraction requires elements that appeared after the learned deadline.

**Deliverable:** p50 settle time drops on simple sites without regressing complex ones.

### Phase G — Cross-session prior persistence + advanced eviction (1 week)

**Goal:** Make all the above work survive process restart and benefit from accumulated learning.

- Already partly in B for opt-in persistent stores. This phase:
  - Replaces LRU bytecode eviction with expected-future-value
  - Adds prior-bundling: ship a "starter pack" of priors for top-1000 domains
  - Adds `unbrowser export-priors` / `import-priors` CLI for sharing/auditing

**Deliverable:** Fresh installs perform near-warmed-up performance via shipped priors.

### Phase H — (optional, post-v1) Speculative preload

**Goal:** Use link-graph priors to preload likely next navigations.

- Per-domain Markov chain over link patterns
- On navigate, prefetch top-K likely next URLs (HEAD or partial GET)
- Background bytecode-precompile of cached scripts likely to be hit

Skip unless Phase A–G results justify the additional complexity.

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

| Metric | What it measures | Target |
|---|---|---|
| `wall_clock_to_blockmap_ms` | Time from navigate to inline blockmap returned | -50% median, -70% p90 vs baseline |
| `wall_clock_to_settle_ms` | Time from navigate to declared settled | -60% median |
| `scripts_executed_count` | Number of `<script>` tags actually run | -50% on commercial sites, ≤baseline elsewhere |
| `scripts_skipped_count` | Number skipped by policy | report only |
| `extraction_success_rate` | Did agent task complete? | ≥ 99% of baseline |
| `blockmap_completeness` | # interactives + # headings detected | ≥ 95% of baseline |
| `query_result_accuracy` | For 10 fixed queries per site | ≥ 99% of baseline |
| `bytecode_cache_hit_rate` | Hits / (hits + misses) | ≥ 80% on second+ visit |
| `stealth_signal_count` | Bot-detection probes that fire | ≤ baseline (no regressions) |
| `binary_size_mb` | Stripped release binary | unchanged (no new bundled engines) |

### 7.3 Configurations

Three configurations, paired across all targets:

- **C0 (baseline)**: current `main`, no policy
- **C1 (observability only)**: Phase A enabled, no behavior-changing decisions. This must be performance-neutral enough to leave enabled for debugging.
- **C2 (cache + settle)**: bytecode cache + settle learner (Phases C, F)
- **C3 (full)**: all phases A–G

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

The implementation ships when:

- C1 vs C0: median wall-clock regression < 5% (observability is cheap enough)
- C3 vs C0: median wall-clock improvement ≥ 50% across the replay corpus, lower CI bound > 30%
- C3 vs C0: extraction success rate change is in `[-1%, +∞)` (i.e., no meaningful regression)
- C3 vs C0: stealth signal count is in `(-∞, +0)` (no new detections)
- C2 alone: at least 30% wall-clock improvement (sanity check that cache and settle learner pull weight)
- All probabilistic state is migratable across schema versions

If any criterion fails, ship the components that pass and revisit the failing one.

### 7.6 Adversarial testing

Beyond corpus performance, three adversarial tests:

1. **Profile poisoning**: simulate a site that changes its bundle. Verify posterior decay catches the drift within N visits.
2. **Stealth probe**: load fingerprint.com or similar. Verify policy-skipped scripts don't include fingerprinting probes the page expects.
3. **Cold-start performance**: fresh install with no priors. Verify performance is no worse than C0 (uninformative priors should default to baseline behavior).

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

1. **Perf**: ≥ 50% median wall-clock improvement on the corpus (target: 70%, ceiling: ~85% — past that we're in the JIT-only territory).
2. **Quality**: Extraction success rate within 1% of baseline.
3. **Stealth**: No new detections on commodity-tier bot-protected sites.
4. **Maintainability**: Total LOC added < 5,000 (Rust + JS combined).
5. **Engine independence**: Policy layer is engine-agnostic — could swap to PrimJS or future QuickJS-NG without policy changes.
6. **Auditable**: Posterior store is human-readable; any decision can be traced to its evidence.

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
