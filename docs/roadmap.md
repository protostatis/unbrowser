# unbrowser roadmap

Operational companion to `docs/probabilistic-policy.md` (the architecture
white paper). The white paper describes what the system *is*; this file
tracks what's *built* and what's *next*.

Last updated: 2026-05-03 (v0.0.6 release).

## Where we are

### v0.0.6 — shipped 2026-05-03

The Bayesian prefit pipeline is operational end-to-end:

| Phase | Status | Where |
|---|---|---|
| **T1** corpus collection | ✅ | `train/collect.py` — 8-way parallel, 103-site curated corpus, retry-once on timeout/crash, categorised outcomes |
| **T2** posterior aggregation | ✅ (proxy) | `train/aggregate.py` — fits Beta(α, β) per (domain, decision) using "scripts.executed > 0" as the success proxy. **Open:** wire in PR #15's `outcome_for_decision` events for the real signal |
| **T3** bundle packing | ✅ | `train/pack.py` — schema v2 with per-domain `posteriors` field |
| **R1** runtime loader | ✅ | `src/prefit.rs` — accepts schema v1 or v2, back-compat path preserves v0 deterministic-block behaviour when posteriors are missing |
| **R2** runtime inference | ✅ | `src/prefit.rs::decide_traced` — Thompson sampling via Marsaglia-Tsang gamma + inline Box-Muller. Gated on `n ≥ 5` so under-trained posteriors don't trigger (preserves v0 behaviour as a safety floor) |

Bundle as shipped: 91 domains × 160 posteriors, all `n ≥ 5`, fitted from
5 corpus passes (~455 navigates). End-to-end Thompson sampling verified
on cnbc.com → `posterior_consulted block:zephr-templates.cnbc.com α=6
β=1 n=5 sampled=0.868 → blocked=true`.

### Other v0.0.6 wins (15-site comparison vs v0.0.5, warm-cache)

- **Verge**: was hanging at 30s in v0.0.5, now ~6s with 251 links + policy blocking 7 trackers
- **YouTube**: was hanging in v0.0.5, now ~1s with 38 scripts executed
- **CNBC**: -2.3s (-50%); **NYT**: -2.6s (-48%); **StackOverflow**: -2.6s (-31%)
- **SPA content**: extract field surfaces real app data on Polymarket (next_data with markets), Tailwind/Next.js homepage (rsc_payload), Verge/NYT/CNBC/Medium (json_ld)
- **Network capture**: NYT now surfaces 3 captured XHR responses
- **76 unit tests** pass (was 38 at start of session)

## What's open

### Polish on what we already shipped

These tighten the existing system without new architecture:

1. **Wire `outcome_for_decision` into T2.** The aggregator currently uses
   the proxy "did any script execute?" — wiring in the rich outcome events
   from PR #15 (with the success/failure heuristic over extract / blockmap
   / network captures) would tighten posteriors meaningfully.
2. **Framework detection at runtime.** All `settle_fast:` posteriors landed
   keyed as `_unknown` because the runtime doesn't sniff React/Next/Vue
   yet. A small JS-side detector (`__NEXT_DATA__`, `__NUXT__`, `Vue`, etc.)
   would let per-framework priors actually fire.
3. **Bundle compression (Wave 2 item #6).** MessagePack + gzip per spec
   §6.5. Current 91-domain JSON bundle is ~50KB. At 1000+ domains it'd
   push past 1MB inline.

### Roadmap not started — closes the white paper's loop

These are the next-phase architecture pieces:

1. **U1: per-user posterior overlay.** Agents accumulate their own
   per-domain priors that layer on top of the shipped bundle. The
   "self-improving" claim of the architecture only holds once this lands.
   Demo would be: same site improves over 5 navigates as outcomes flow
   back into the local overlay.
2. **U2: contribution back to the corpus.** Per-user overlays opt-in
   upstream to grow the shared bundle. Needs U1 first plus a privacy
   story for what gets uploaded.

### Crawl / data scaling (multi-week)

Our 103-site corpus produces `n = 5-10` posteriors, which is enough to
trigger Thompson but not enough to tighten the credible interval much.
For meaningful tightening (`n ≥ 50`), we need a bigger corpus and more
passes:

- **1000+ site corpus.** Top-N from Tranco, weighted toward commercial /
  news / e-commerce. Current 103-site seed file lives at
  `train/corpus/seed_sites.json`.
- **IP rotation.** WAFs at Vercel / CloudFront-style edges already
  rate-limit us at 100 sites in 80s. Scaling needs residential or
  rotating proxy support.
- **Stealth Phase 4 (CLAUDE.md).** Profile system
  (`profiles/chrome_*.toml`), fingerprint test harness against a known
  FP-detection corpus. Necessary for the corpus scaling above.

### Performance regressions still open

- **Vercel intermittent hang.** Works ~67% of the time; not reproducible
  on demand, likely WAF-side. Not actionable from our code.
- **Tailwind / Next.js homepage per-script tax (+5s).** Settle exits
  cleanly with `iters=0` — the cost is in the script-eval phase. Probably
  fixable by skipping observer microtask drains between scripts during
  the script-execution phase.
- **Bluesky / true CSR full mount.** Bundle progresses ~3s into
  hydration before bailing on a generic `TypeError: not a function`.
  React-native-web has a deep tail of missing globals.

## Order-of-attack recommendation

If you want the next demo to show **better priors**: do the T2 outcome
wiring first (~half day) — same bundle shape, sharper posteriors.

If you want the next demo to show **a new capability**: do U1 — agents
get visibly better at flaky sites over a session. This is the real
"self-improving" headline.

If you want a **release that scales**: do crawl scaling. Bigger corpus +
better posteriors + bundle compression land together as a coherent v0.0.7.

## Release process (record so we don't re-discover it next time)

For tagging a new binary release:

1. Bump **both** `Cargo.toml` and `python/pyproject.toml` to the new
   version. Forgetting the second one breaks PyPI publish (file already
   exists).
2. Commit. Push to main.
3. `git tag -a vX.Y.Z -m "..."` and `git push origin vX.Y.Z`.
4. The `release.yml` workflow runs on tag push: builds 3 platform
   binaries, creates the GitHub Release with tarballs + sha256s,
   publishes wheels + sdist to PyPI via OIDC trusted publishing.
5. Wait ~1-2 minutes for PyPI CDN to propagate
   (`https://pypi.org/pypi/pyunbrowser/<version>/json`).

For bumping the **skill** version (separate from binary version):

1. Edit `skills/unbrowser/SKILL.md` — bump `version:` frontmatter and
   the content. Commit + push to main.
2. **Manually** publish to ClawHub:
   `clawhub publish skills/unbrowser --version X.Y.Z --changelog "..."`.
   ClawHub does NOT auto-poll GitHub; the publish step is required.

The binary version (Cargo.toml) and skill version (SKILL.md frontmatter)
move independently — the skill can iterate without a binary release.
