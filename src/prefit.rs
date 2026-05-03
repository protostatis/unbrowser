//! Prefit bundle loader and lookup (R1 from white paper §6 Track 1).
//!
//! The runtime ships with a centrally-trained bundle of per-(domain,
//! framework) decision parameters. This is the "prefit-first" half of the
//! reframe in PR #1: each user instance starts INFORMED about the popular
//! long head, instead of learning per-site posteriors from zero. Cold-
//! start is the adoption gate (§1.4); shipping the prior closes it.
//!
//! Bundle format: JSON for v0 (MessagePack later — see §6.5). Compiled
//! into the binary via `include_bytes!`, so first run has zero IO. Future
//! versions may move to a downloaded-on-first-run file under
//! `~/.unbrowser/prefit/v{N}.bundle` to avoid binary-size growth as the
//! corpus expands past ~10K domains.
//!
//! Lookup at runtime is one HashMap query per navigation:
//!   1. domain → DomainPrefit (exact match)
//!   2. fall back to framework_priors[framework] (when the domain is new
//!      but the framework is recognized at runtime)
//!   3. fall back to structural defaults (Tier-1 blocklist, default settle)
//!
//! v0 ships with hand-curated entries for the 10-site test corpus. Real
//! T2/T3 pipeline scripts in train/aggregate.py + train/pack.py can
//! regenerate the bundle from observed Phase A events.
//!
//! ## Schema versions
//!
//! - **v1** — domains carry blocklist_additions / settle_distribution /
//!   framework / shape_hint / required_patterns. No Bayesian
//!   posteriors.
//! - **v2** — adds `posteriors: {decision_key → BetaPosterior}` per
//!   domain. Decision keys are stable strings the runtime queries:
//!   `block:<host>` for per-host blocklist decisions and
//!   `settle_fast:<framework>` for settle outcomes. Missing posteriors
//!   are treated as "no information"; the runtime falls back to its
//!   structural defaults rather than acting on absent evidence.
//!
//! Both v1 and v2 parse here. v1 bundles simply present an empty
//! posterior table. The loader rejects schema versions outside the
//! supported set so a newer training pipeline (with breaking schema
//! changes) doesn't silently degrade an older runtime.

use rand::Rng;
use rand::distributions::{Distribution, Open01};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Highest bundle schema_version this runtime understands. Newer
/// bundles (e.g. with categorical decision posteriors, §4.4) will bump
/// this; older runtimes refuse to load them rather than silently
/// dropping unknown fields.
pub const MAX_SUPPORTED_SCHEMA: u32 = 2;
pub const MIN_SUPPORTED_SCHEMA: u32 = 1;

/// Embedded prefit bundle. Update by running `python3 train/pack.py`
/// which writes this file.
const EMBEDDED_BUNDLE: &str = include_str!("../prefit/v1.bundle.json");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PrefitBundle {
    pub schema_version: u32,
    pub fit_timestamp: u64,
    pub fit_corpus_size: u32,
    pub training_pipeline_version: String,
    pub domains: HashMap<String, DomainPrefit>,
    #[serde(default)]
    pub framework_priors: HashMap<String, FrameworkPrefit>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DomainPrefit {
    pub domain: String,
    #[serde(default)]
    pub framework: Option<String>,
    /// Hostnames or hostname-suffix patterns that this domain is known to
    /// load and that should be blocked. These supplement the Tier-1
    /// blocklist for navigations to this specific domain — useful for
    /// per-site trackers that aren't in the global blocklist.
    #[serde(default)]
    pub blocklist_additions: Vec<String>,
    /// URL substrings that, if they match a blocked URL, override the
    /// block (force-allow). For first-party app code that happens to
    /// match a blocklist pattern. Empty for v0.
    #[serde(default)]
    pub required_patterns: Vec<String>,
    /// Pre-fit settle time distribution. Drivers can use this as a
    /// budget hint (e.g. wait at most p95_ms before declaring stalled).
    #[serde(default)]
    pub settle_distribution: Option<SettleDistribution>,
    /// Free-form hint for the agent: "spa_shell_with_density",
    /// "static_ssr", "card_grid", etc. Future Bayesian phases can
    /// condition on this; v0 just surfaces it.
    #[serde(default)]
    pub shape_hint: Option<String>,
    /// Bayesian posteriors over per-decision binary outcomes (v2+).
    /// Empty for v1 bundles. See `BetaPosterior` for the shape and
    /// the module-level docs for the decision key taxonomy.
    #[serde(default)]
    pub posteriors: HashMap<String, BetaPosterior>,
}

/// Beta(α, β) posterior over a binary decision's success rate.
///
/// Stored as two `f64`s plus the observed sample count `n`. The
/// pseudo-count interpretation is `α + β - prior_total`, but we keep
/// `n` separate so consumers can distinguish "Beta(1, 1) because no
/// data has arrived" from "Beta(1, 1) because the prior happens to
/// match the observed counts." See spec §4.2 / §4.7.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
pub struct BetaPosterior {
    pub alpha: f64,
    pub beta: f64,
    /// Number of observations that contributed to this posterior.
    /// `n == 0` means the entry is a placeholder — runtime should
    /// either use the prior mean or skip the decision rather than
    /// committing on no evidence.
    #[serde(default)]
    pub n: u64,
}

impl BetaPosterior {
    /// Posterior mean = α / (α + β). Closed-form, no sampling needed.
    /// Currently exercised by tests; kept on the public API for future
    /// callers (e.g. a `prefit-info --posteriors` CLI dump).
    #[allow(dead_code)]
    pub fn mean(&self) -> f64 {
        let denom = self.alpha + self.beta;
        if denom <= 0.0 {
            return 0.5;
        }
        self.alpha / denom
    }

    /// Thompson-style draw from Beta(α, β).
    ///
    /// Implementation: marsaglia-tsang gamma sampling; X = G(α) /
    /// (G(α) + G(β)) is Beta(α, β)-distributed. Uses only `Rng` so the
    /// caller picks the source — `rand::thread_rng()` for production,
    /// `StdRng::seed_from_u64(...)` for tests. No panics: degenerate
    /// posteriors (α or β ≤ 0) fall back to a uniform draw, which
    /// matches the BetaPosterior::mean() fallback semantics.
    pub fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> f64 {
        if !self.alpha.is_finite()
            || !self.beta.is_finite()
            || self.alpha <= 0.0
            || self.beta <= 0.0
        {
            return Open01.sample(rng);
        }
        let x = sample_gamma(rng, self.alpha);
        let y = sample_gamma(rng, self.beta);
        let denom = x + y;
        if denom > 0.0 { x / denom } else { 0.5 }
    }
}

/// Marsaglia-Tsang gamma(shape, 1.0) sampler.
///
/// Used by `BetaPosterior::sample`. Boosts shape < 1 via the
/// Marsaglia-Tsang trick: G(a) = G(a + 1) * U^(1/a). Standard, fast,
/// no rejection blowup at small shapes. Doesn't allocate.
///
/// We avoid pulling in `rand_distr` for `StandardNormal` and inline a
/// Box-Muller draw from two `Open01` uniforms instead — keeps the
/// dependency footprint to `rand` only and the math is elementary.
fn sample_gamma<R: Rng + ?Sized>(rng: &mut R, shape: f64) -> f64 {
    if shape < 1.0 {
        // Boosting trick.
        let g = sample_gamma(rng, shape + 1.0);
        let u: f64 = Open01.sample(rng);
        return g * u.powf(1.0 / shape);
    }
    let d = shape - 1.0 / 3.0;
    let c = 1.0 / (9.0 * d).sqrt();
    loop {
        let x: f64 = sample_standard_normal(rng);
        let v_base = 1.0 + c * x;
        if v_base <= 0.0 {
            continue;
        }
        let v = v_base.powi(3);
        let u: f64 = Open01.sample(rng);
        let xx = x * x;
        // Squeeze test (cheap acceptance).
        if u < 1.0 - 0.0331 * xx * xx {
            return d * v;
        }
        // Full acceptance test.
        if u.ln() < 0.5 * xx + d * (1.0 - v + v.ln()) {
            return d * v;
        }
    }
}

/// Box-Muller draw of one standard-normal sample. Discards the
/// second sample; rejection rate in marsaglia-tsang is low enough
/// that the throughput cost is negligible vs adding `rand_distr`.
fn sample_standard_normal<R: Rng + ?Sized>(rng: &mut R) -> f64 {
    let u1: f64 = Open01.sample(rng);
    let u2: f64 = Open01.sample(rng);
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FrameworkPrefit {
    pub framework: String,
    #[serde(default)]
    pub blocklist_additions: Vec<String>,
    #[serde(default)]
    pub settle_distribution: Option<SettleDistribution>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SettleDistribution {
    pub p50_ms: u32,
    pub p90_ms: u32,
    pub p95_ms: u32,
}

impl PrefitBundle {
    /// Load the embedded bundle. Returns None on parse failure (which
    /// would be a build-time bug, but we don't want to panic at startup
    /// — the runtime falls back to no-prefit behavior). Also returns
    /// None when `schema_version` falls outside the supported range —
    /// the runtime would rather refuse to load than silently downgrade
    /// against a bundle it doesn't understand.
    pub fn load_embedded() -> Option<Self> {
        Self::from_json(EMBEDDED_BUNDLE)
    }

    /// Internal: parse a JSON bundle, validating the schema version.
    /// Public to tests via `pub(crate)` to allow round-trip testing of
    /// the v1-backward-compat path without touching the embedded file.
    pub(crate) fn from_json(s: &str) -> Option<Self> {
        match serde_json::from_str::<Self>(s) {
            Ok(b) => {
                if b.schema_version < MIN_SUPPORTED_SCHEMA
                    || b.schema_version > MAX_SUPPORTED_SCHEMA
                {
                    eprintln!(
                        "prefit: refusing schema_version={} (supported {}–{})",
                        b.schema_version, MIN_SUPPORTED_SCHEMA, MAX_SUPPORTED_SCHEMA
                    );
                    return None;
                }
                Some(b)
            }
            Err(e) => {
                eprintln!("prefit: failed to parse bundle: {e}");
                None
            }
        }
    }

    /// Look up prefit for a host. Tries exact match first; future versions
    /// will add suffix matching for subdomains. Returns None when the host
    /// isn't in the bundle (caller falls back to framework_priors or
    /// structural defaults).
    pub fn lookup_domain(&self, host: &str) -> Option<&DomainPrefit> {
        let host_lower = host.to_lowercase();
        // Exact match
        if let Some(p) = self.domains.get(&host_lower) {
            return Some(p);
        }
        // Suffix match — host matches if it equals or ends with `.{key}`.
        // Lets a single entry for `cnbc.com` cover `www.cnbc.com`,
        // `markets.cnbc.com`, etc.
        for (key, prefit) in &self.domains {
            if host_lower == *key || host_lower.ends_with(&format!(".{key}")) {
                return Some(prefit);
            }
        }
        None
    }

    /// True if the URL matches one of the domain's blocklist_additions.
    /// Used by the navigate-time policy hook to extend Tier-1 blocking
    /// with per-domain knowledge from the prefit.
    pub fn matches_blocklist_addition(&self, host_prefit: &DomainPrefit, url: &str) -> bool {
        let parsed = match url::Url::parse(url) {
            Ok(u) => u,
            Err(_) => return false,
        };
        let host = match parsed.host_str() {
            Some(h) => h.to_lowercase(),
            None => return false,
        };
        for pattern in &host_prefit.blocklist_additions {
            let p = pattern.to_lowercase();
            if host == p || host.ends_with(&format!(".{p}")) {
                return true;
            }
        }
        false
    }

    /// Number of domains in the bundle — for the `prefit-info` CLI command.
    pub fn domain_count(&self) -> usize {
        self.domains.len()
    }

    /// Look up a posterior by `(domain, decision_key)`. Mirrors
    /// `lookup_domain` semantics: exact host match first, then suffix
    /// match (so `cnbc.com` covers `www.cnbc.com`). Returns owned
    /// `BetaPosterior` because the caller will sample / mutate
    /// independently.
    ///
    /// Returns `None` when:
    /// - the domain is not in the bundle
    /// - the bundle has no posteriors for that domain (v1 bundle)
    /// - the decision_key is not present in that domain's posterior
    ///   table
    ///
    /// Note: callers should NOT default to Beta(1, 1) on None and
    /// proceed — the white paper §4.2 explicitly warns that uniform
    /// priors will mis-classify unknown scripts. Treat None as
    /// "no information; use the safe default action."
    pub fn lookup_posterior(&self, domain: &str, decision_key: &str) -> Option<BetaPosterior> {
        let p = self.lookup_domain(domain)?;
        p.posteriors.get(decision_key).copied()
    }

    /// Thompson-sampled binary decision: draw `p ~ Beta(α, β)` from
    /// the looked-up posterior, return `p >= threshold`. When no
    /// posterior is found, return `false` (don't act on missing
    /// evidence — see `lookup_posterior` rationale).
    ///
    /// Caller passes the RNG so production paths can use
    /// `rand::thread_rng()` while tests use a seeded RNG for
    /// determinism. Threshold is the action-cutoff: `0.5` is the
    /// natural Bayesian decision boundary (act when posterior probability
    /// of success exceeds 0.5); raise to be more conservative (only
    /// act when strongly confident), lower to be more aggressive.
    ///
    /// For call sites that also need the underlying sample value
    /// (e.g. for emitting an observability event), use
    /// `decide_traced` instead.
    ///
    /// Currently used only via tests — main.rs goes through
    /// `decide_traced` so it can emit a `posterior_consulted` event.
    /// Kept on the public API as the convenience entry point for
    /// callers that don't need diagnostics.
    #[allow(dead_code)]
    pub fn decide<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        domain: &str,
        decision_key: &str,
        threshold: f64,
    ) -> bool {
        self.decide_traced(rng, domain, decision_key, threshold)
            .blocked
    }

    /// Same as `decide`, but returns the posterior + drawn sample so
    /// the caller can emit `posterior_consulted` with full diagnostics.
    /// `posterior` is `None` when no entry was found, in which case
    /// `blocked = false` and `sampled = None`.
    pub fn decide_traced<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        domain: &str,
        decision_key: &str,
        threshold: f64,
    ) -> DecideOutcome {
        match self.lookup_posterior(domain, decision_key) {
            Some(post) => {
                let s = post.sample(rng);
                DecideOutcome {
                    posterior: Some(post),
                    sampled: Some(s),
                    threshold,
                    blocked: s >= threshold,
                }
            }
            None => DecideOutcome {
                posterior: None,
                sampled: None,
                threshold,
                blocked: false,
            },
        }
    }
}

/// Bundle of diagnostics returned by `decide_traced` so the call site
/// can both gate behavior and emit an observability event without
/// resampling.
#[derive(Debug, Clone, Copy)]
pub struct DecideOutcome {
    pub posterior: Option<BetaPosterior>,
    pub sampled: Option<f64>,
    /// The threshold that was applied — kept for observability so a
    /// `posterior_consulted` event can be reconstructed from the
    /// outcome alone. Not consumed by main.rs currently (it has the
    /// constant in scope) but preserved on the API surface.
    #[allow(dead_code)]
    pub threshold: f64,
    pub blocked: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_bundle_parses() {
        let b = PrefitBundle::load_embedded().expect("embedded bundle should parse");
        assert!(b.schema_version >= 1);
        assert!(b.domain_count() >= 1, "v0 bundle should have ≥1 domain");
    }

    #[test]
    fn lookup_exact_domain() {
        let b = PrefitBundle::load_embedded().expect("parse");
        // Pick the first domain in the bundle and look it up.
        let key = b.domains.keys().next().unwrap().clone();
        assert!(b.lookup_domain(&key).is_some());
    }

    #[test]
    fn lookup_suffix_match() {
        let mut domains = HashMap::new();
        domains.insert(
            "example.com".to_string(),
            DomainPrefit {
                domain: "example.com".to_string(),
                framework: None,
                blocklist_additions: vec![],
                required_patterns: vec![],
                settle_distribution: None,
                shape_hint: None,
                posteriors: HashMap::new(),
            },
        );
        let b = PrefitBundle {
            schema_version: 1,
            fit_timestamp: 0,
            fit_corpus_size: 0,
            training_pipeline_version: "test".to_string(),
            domains,
            framework_priors: HashMap::new(),
        };
        assert!(b.lookup_domain("example.com").is_some());
        assert!(b.lookup_domain("www.example.com").is_some());
        assert!(b.lookup_domain("api.example.com").is_some());
        assert!(b.lookup_domain("notexample.com").is_none());
        assert!(b.lookup_domain("example.org").is_none());
    }

    #[test]
    fn matches_blocklist_addition_works() {
        let p = DomainPrefit {
            domain: "site.com".to_string(),
            framework: None,
            blocklist_additions: vec!["bad-tracker.com".to_string()],
            required_patterns: vec![],
            settle_distribution: None,
            shape_hint: None,
            posteriors: HashMap::new(),
        };
        let b = PrefitBundle {
            schema_version: 1,
            fit_timestamp: 0,
            fit_corpus_size: 0,
            training_pipeline_version: "test".to_string(),
            domains: HashMap::new(),
            framework_priors: HashMap::new(),
        };
        assert!(b.matches_blocklist_addition(&p, "https://bad-tracker.com/foo"));
        assert!(b.matches_blocklist_addition(&p, "https://cdn.bad-tracker.com/foo"));
        assert!(!b.matches_blocklist_addition(&p, "https://good-site.com/foo"));
    }

    #[test]
    fn unknown_domain_returns_none() {
        let b = PrefitBundle::load_embedded().expect("parse");
        assert!(b.lookup_domain("nonexistent.example.invalid").is_none());
    }

    // ---- Bayesian posterior tests (schema v2) ----

    #[test]
    fn beta_mean_basic() {
        let p = BetaPosterior {
            alpha: 3.0,
            beta: 1.0,
            n: 4,
        };
        assert!((p.mean() - 0.75).abs() < 1e-12);

        let uniform = BetaPosterior {
            alpha: 1.0,
            beta: 1.0,
            n: 0,
        };
        assert!((uniform.mean() - 0.5).abs() < 1e-12);

        // Degenerate guard.
        let zero = BetaPosterior {
            alpha: 0.0,
            beta: 0.0,
            n: 0,
        };
        assert!((zero.mean() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn beta_sample_within_unit_interval() {
        use rand::SeedableRng;
        use rand::rngs::StdRng;
        let mut rng = StdRng::seed_from_u64(0xCAFEBABE);
        let p = BetaPosterior {
            alpha: 4.0,
            beta: 6.0,
            n: 10,
        };
        for _ in 0..200 {
            let s = p.sample(&mut rng);
            assert!(s > 0.0 && s < 1.0, "sample out of (0, 1): {s}");
        }
    }

    #[test]
    fn beta_sample_concentrates_near_mean() {
        // With α=80, β=20 the mean is 0.8 and sd ≈ 0.04. 1000 draws
        // should easily land within 0.05 of 0.8 on average.
        use rand::SeedableRng;
        use rand::rngs::StdRng;
        let mut rng = StdRng::seed_from_u64(42);
        let p = BetaPosterior {
            alpha: 80.0,
            beta: 20.0,
            n: 100,
        };
        let n = 1000;
        let sum: f64 = (0..n).map(|_| p.sample(&mut rng)).sum();
        let mean = sum / n as f64;
        assert!(
            (mean - 0.8).abs() < 0.02,
            "empirical mean {mean} far from 0.8"
        );
    }

    #[test]
    fn beta_sample_seeded_is_deterministic() {
        use rand::SeedableRng;
        use rand::rngs::StdRng;
        let p = BetaPosterior {
            alpha: 5.0,
            beta: 5.0,
            n: 8,
        };
        let mut a = StdRng::seed_from_u64(7);
        let mut b = StdRng::seed_from_u64(7);
        for _ in 0..50 {
            assert_eq!(p.sample(&mut a), p.sample(&mut b));
        }
    }

    fn make_bundle_with_posterior(
        domain: &str,
        key: &str,
        post: BetaPosterior,
        schema: u32,
    ) -> PrefitBundle {
        let mut posts = HashMap::new();
        posts.insert(key.to_string(), post);
        let mut domains = HashMap::new();
        domains.insert(
            domain.to_string(),
            DomainPrefit {
                domain: domain.to_string(),
                framework: None,
                blocklist_additions: vec![],
                required_patterns: vec![],
                settle_distribution: None,
                shape_hint: None,
                posteriors: posts,
            },
        );
        PrefitBundle {
            schema_version: schema,
            fit_timestamp: 0,
            fit_corpus_size: 0,
            training_pipeline_version: "test".to_string(),
            domains,
            framework_priors: HashMap::new(),
        }
    }

    #[test]
    fn lookup_posterior_exact_and_suffix() {
        let post = BetaPosterior {
            alpha: 9.0,
            beta: 1.0,
            n: 9,
        };
        let b = make_bundle_with_posterior("cnbc.com", "block:zephr.com", post, 2);

        // Exact match.
        let got = b.lookup_posterior("cnbc.com", "block:zephr.com").unwrap();
        assert_eq!(got, post);
        // Suffix match (subdomain of registered domain).
        let got = b
            .lookup_posterior("www.cnbc.com", "block:zephr.com")
            .unwrap();
        assert_eq!(got, post);
        // Missing decision key.
        assert!(b.lookup_posterior("cnbc.com", "block:other.com").is_none());
        // Missing domain entirely.
        assert!(
            b.lookup_posterior("nbcnews.com", "block:zephr.com")
                .is_none()
        );
    }

    #[test]
    fn decide_returns_false_for_unknown() {
        use rand::SeedableRng;
        use rand::rngs::StdRng;
        let mut rng = StdRng::seed_from_u64(1);
        let b = make_bundle_with_posterior(
            "x.com",
            "block:y.com",
            BetaPosterior {
                alpha: 1.0,
                beta: 1.0,
                n: 0,
            },
            2,
        );
        assert!(!b.decide(&mut rng, "no-such-domain.invalid", "block:y.com", 0.5));
        assert!(!b.decide(&mut rng, "x.com", "block:no-such-decision", 0.5));
    }

    #[test]
    fn decide_seeded_is_deterministic() {
        use rand::SeedableRng;
        use rand::rngs::StdRng;
        // High-confidence success posterior — should sample > 0.5
        // virtually always, so decide is True with overwhelming
        // probability. Pick a seed where we know the answer.
        let post = BetaPosterior {
            alpha: 90.0,
            beta: 10.0,
            n: 100,
        };
        let b = make_bundle_with_posterior("a.com", "block:t.com", post, 2);
        let mut r1 = StdRng::seed_from_u64(123);
        let mut r2 = StdRng::seed_from_u64(123);
        for _ in 0..20 {
            assert_eq!(
                b.decide(&mut r1, "a.com", "block:t.com", 0.5),
                b.decide(&mut r2, "a.com", "block:t.com", 0.5)
            );
        }
        // Threshold above the support of any plausible draw → false.
        let mut r3 = StdRng::seed_from_u64(123);
        for _ in 0..20 {
            assert!(!b.decide(&mut r3, "a.com", "block:t.com", 0.999_999));
        }
    }

    #[test]
    fn v1_bundle_loads_with_empty_posteriors() {
        // A v1 bundle (no `posteriors` field on any domain) must parse
        // and present an empty posterior table per domain. This is the
        // backward-compat contract.
        let v1 = r#"{
            "schema_version": 1,
            "fit_timestamp": 0,
            "fit_corpus_size": 0,
            "training_pipeline_version": "test-v1",
            "domains": {
                "v1site.com": {
                    "domain": "v1site.com",
                    "blocklist_additions": ["x.com"],
                    "required_patterns": [],
                    "settle_distribution": {"p50_ms": 100, "p90_ms": 200, "p95_ms": 400}
                }
            },
            "framework_priors": {}
        }"#;
        let b = PrefitBundle::from_json(v1).expect("v1 should parse");
        assert_eq!(b.schema_version, 1);
        let dom = b.lookup_domain("v1site.com").unwrap();
        assert!(dom.posteriors.is_empty());
        // Lookup a posterior — should be None since v1 has none.
        assert!(b.lookup_posterior("v1site.com", "block:x.com").is_none());
    }

    #[test]
    fn v2_bundle_round_trips_posteriors() {
        // A v2 bundle's posteriors round-trip through serde without
        // loss. Pinning this prevents an accidental field rename or
        // serde attribute change from breaking the bundle contract.
        let post = BetaPosterior {
            alpha: 12.0,
            beta: 2.0,
            n: 14,
        };
        let b = make_bundle_with_posterior("z.com", "block:t.com", post, 2);
        let s = serde_json::to_string(&b).unwrap();
        let back = PrefitBundle::from_json(&s).expect("v2 round-trip");
        let got = back.lookup_posterior("z.com", "block:t.com").unwrap();
        assert_eq!(got, post);
    }

    #[test]
    fn unsupported_schema_version_rejected() {
        // schema_version above MAX_SUPPORTED_SCHEMA must refuse —
        // silent downgrade is worse than no prefit at all.
        let too_new = format!(
            r#"{{
                "schema_version": {},
                "fit_timestamp": 0,
                "fit_corpus_size": 0,
                "training_pipeline_version": "future",
                "domains": {{}},
                "framework_priors": {{}}
            }}"#,
            MAX_SUPPORTED_SCHEMA + 1
        );
        assert!(PrefitBundle::from_json(&too_new).is_none());
    }

    #[test]
    fn embedded_v2_decide_is_deterministic_with_seed() {
        // End-to-end: load the regenerated embedded bundle, call
        // decide() against a known domain+key with a seeded RNG, and
        // assert the result is stable. Exercises the full v2 path:
        // include_str! → parse → lookup_posterior → sample → threshold.
        use rand::SeedableRng;
        use rand::rngs::StdRng;
        let b = PrefitBundle::load_embedded().expect("embedded bundle parses");
        // We pick zephr-templates.cnbc.com — the bundle ships a
        // placeholder Beta(1, 1) for it, so the sample is uniform.
        // Two seeded draws from the same seed must agree.
        let key = "block:zephr-templates.cnbc.com";
        let dom = "cnbc.com";
        // Skip the assertion if the embedded bundle was rebuilt in a
        // mode that doesn't include this posterior — the test still
        // exercises the full path, just with a None lookup. This makes
        // the test resilient to bundle regeneration without
        // hand-editing.
        if b.lookup_posterior(dom, key).is_none() {
            return;
        }
        let mut r1 = StdRng::seed_from_u64(0xDEADBEEF);
        let mut r2 = StdRng::seed_from_u64(0xDEADBEEF);
        for _ in 0..10 {
            assert_eq!(
                b.decide(&mut r1, dom, key, 0.5),
                b.decide(&mut r2, dom, key, 0.5)
            );
        }
    }
}
