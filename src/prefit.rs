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

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    /// — the runtime falls back to no-prefit behavior).
    pub fn load_embedded() -> Option<Self> {
        match serde_json::from_str::<Self>(EMBEDDED_BUNDLE) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("prefit: failed to parse embedded bundle: {e}");
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
}
