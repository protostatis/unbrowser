//! Execution policy: deterministic blocklist for analytics, ads, replay,
//! pixels, and consent CMPs. Categories where the answer is always "skip"
//! for an LLM agent — no posterior to learn, no inference to run.
//!
//! Hooks into `__host_fetch_send` in main.rs: when JS-issued fetch goes to
//! a blocked host, we short-circuit with a synthetic 204 instead of making
//! the request. Catches both static `<script src>` and runtime-injected
//! tracker URLs (the bulk of trackers on commercial sites are runtime-
//! injected by tag managers, so the fetch layer is the right hook, not the
//! `<script>` tag layer).
//!
//! Match algorithm: host suffix. A host matches a pattern if it equals the
//! pattern or ends with `.{pattern}`. Exact-match short-circuited via
//! HashSet; suffix walk for the rest.
//!
//! See docs/probabilistic-policy.md for the broader framework. This module
//! implements Tier 1 (deterministic block); Tier 2 (Bayesian residual
//! filter) is a separate phase.
//!
//! STEALTH SAFETY: anti-bot / fingerprinting / challenge CDN hosts are
//! intentionally NOT in this blocklist. Their *absence* is detectable —
//! pages that load FingerprintJS, PerimeterX, Datadome, Akamai BMP, hCaptcha,
//! reCAPTCHA, etc. expect those scripts to execute, and missing fetches
//! become a bot signal. The `stealth_safety_no_fingerprinting_hosts` test
//! locks this in.

use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Analytics,
    Ads,
    SessionReplay,
    ErrorBeacon,
    MarketingPixel,
    TagManager,
    ConsentCmp,
}

impl Category {
    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Analytics => "analytics",
            Category::Ads => "ads",
            Category::SessionReplay => "session_replay",
            Category::ErrorBeacon => "error_beacon",
            Category::MarketingPixel => "marketing_pixel",
            Category::TagManager => "tag_manager",
            Category::ConsentCmp => "consent_cmp",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub blocked: bool,
    pub category: Option<Category>,
    pub matched_pattern: Option<&'static str>,
}

impl Decision {
    pub fn allow() -> Self {
        Self {
            blocked: false,
            category: None,
            matched_pattern: None,
        }
    }
    pub fn block(category: Category, pattern: &'static str) -> Self {
        Self {
            blocked: true,
            category: Some(category),
            matched_pattern: Some(pattern),
        }
    }
}

// Hostname suffixes. Source is a mix of:
//   - Top trackers observed in scripts/policy_baseline.py runs
//     (cnbc, forbes, theverge, etc.)
//   - EasyList-derived analytics/ads/tracking hostnames
//   - Top-N entries from the Disconnect tracker list
// Patterns are exact hostnames or suffixes (matched both ways: equality and
// `.{pattern}` suffix). Keep this list visible in source — the auditability
// is the point. If it grows past ~500 entries we can move to a separate file.
const ENTRIES: &[(&str, Category)] = &[
    // -- Tag managers (cascade triggers — blocking these stops dozens of downstream loads)
    ("googletagmanager.com", Category::TagManager),
    ("googletagservices.com", Category::TagManager),
    ("adobedtm.com", Category::TagManager),
    ("assets.adobedtm.com", Category::TagManager),
    ("tags.tiqcdn.com", Category::TagManager),
    ("tealiumiq.com", Category::TagManager),
    ("ensighten.com", Category::TagManager),
    // -- Analytics
    ("google-analytics.com", Category::Analytics),
    ("ssl.google-analytics.com", Category::Analytics),
    ("www.google-analytics.com", Category::Analytics),
    ("stats.g.doubleclick.net", Category::Analytics),
    ("amplitude.com", Category::Analytics),
    ("api.amplitude.com", Category::Analytics),
    ("cdn.amplitude.com", Category::Analytics),
    ("api2.amplitude.com", Category::Analytics),
    ("mixpanel.com", Category::Analytics),
    ("api.mixpanel.com", Category::Analytics),
    ("cdn.mxpnl.com", Category::Analytics),
    ("heap.io", Category::Analytics),
    ("heapanalytics.com", Category::Analytics),
    ("cdn.heapanalytics.com", Category::Analytics),
    ("segment.com", Category::Analytics),
    ("segment.io", Category::Analytics),
    ("cdn.segment.com", Category::Analytics),
    ("api.segment.io", Category::Analytics),
    ("chartbeat.com", Category::Analytics),
    ("chartbeat.net", Category::Analytics),
    ("static.chartbeat.com", Category::Analytics),
    ("ping.chartbeat.net", Category::Analytics),
    ("quantserve.com", Category::Analytics),
    ("scorecardresearch.com", Category::Analytics),
    ("sb.scorecardresearch.com", Category::Analytics),
    ("krxd.net", Category::Analytics),
    ("demdex.net", Category::Analytics),
    ("everesttech.net", Category::Analytics),
    ("optimizely.com", Category::Analytics),
    ("cdn.optimizely.com", Category::Analytics),
    ("vwo.com", Category::Analytics),
    ("dev.visualwebsiteoptimizer.com", Category::Analytics),
    ("branch.io", Category::Analytics),
    ("app.link", Category::Analytics),
    ("cloudflareinsights.com", Category::Analytics),
    ("static.cloudflareinsights.com", Category::Analytics),
    ("plausible.io", Category::Analytics),
    ("cdn.plausible.io", Category::Analytics),
    ("matomo.cloud", Category::Analytics),
    // -- Ads (publisher / programmatic)
    ("doubleclick.net", Category::Ads),
    ("g.doubleclick.net", Category::Ads),
    ("googlesyndication.com", Category::Ads),
    ("googleadservices.com", Category::Ads),
    ("pagead2.googlesyndication.com", Category::Ads),
    ("amazon-adsystem.com", Category::Ads),
    ("aps.amazon-adsystem.com", Category::Ads),
    ("config.aps.amazon-adsystem.com", Category::Ads),
    ("client.aps.amazon-adsystem.com", Category::Ads),
    ("doubleverify.com", Category::Ads),
    ("pub.doubleverify.com", Category::Ads),
    ("cdn.doubleverify.com", Category::Ads),
    ("adsafeprotected.com", Category::Ads),
    ("static.adsafeprotected.com", Category::Ads),
    ("pixel.advertising.com", Category::Ads),
    ("adnxs.com", Category::Ads),
    ("rubiconproject.com", Category::Ads),
    ("openx.net", Category::Ads),
    ("pubmatic.com", Category::Ads),
    ("criteo.com", Category::Ads),
    ("static.criteo.net", Category::Ads),
    ("criteo.net", Category::Ads),
    ("taboola.com", Category::Ads),
    ("cdn.taboola.com", Category::Ads),
    ("outbrain.com", Category::Ads),
    ("widgets.outbrain.com", Category::Ads),
    ("concert.io", Category::Ads),
    ("cdn.concert.io", Category::Ads),
    // -- Session replay / heatmap
    ("hotjar.com", Category::SessionReplay),
    ("static.hotjar.com", Category::SessionReplay),
    ("script.hotjar.com", Category::SessionReplay),
    ("fullstory.com", Category::SessionReplay),
    ("rs.fullstory.com", Category::SessionReplay),
    ("logrocket.com", Category::SessionReplay),
    ("logrocket.io", Category::SessionReplay),
    ("cdn.logrocket.io", Category::SessionReplay),
    ("smartlook.com", Category::SessionReplay),
    ("rec.smartlook.com", Category::SessionReplay),
    ("mouseflow.com", Category::SessionReplay),
    ("inspectlet.com", Category::SessionReplay),
    // -- Error / RUM beacons (low-value to agents; the page works without them)
    ("sentry.io", Category::ErrorBeacon),
    ("ingest.sentry.io", Category::ErrorBeacon),
    ("sentry-cdn.com", Category::ErrorBeacon),
    ("browser.sentry-cdn.com", Category::ErrorBeacon),
    ("datadoghq-browser-agent.com", Category::ErrorBeacon),
    ("browser-intake-datadoghq.com", Category::ErrorBeacon),
    ("newrelic.com", Category::ErrorBeacon),
    ("nr-data.net", Category::ErrorBeacon),
    ("js-agent.newrelic.com", Category::ErrorBeacon),
    ("bam.nr-data.net", Category::ErrorBeacon),
    // -- Marketing pixels
    ("facebook.net", Category::MarketingPixel),
    ("connect.facebook.net", Category::MarketingPixel),
    ("facebook.com/tr", Category::MarketingPixel), // pattern handled in suffix; pixel endpoint
    ("snap.licdn.com", Category::MarketingPixel),
    ("px.ads.linkedin.com", Category::MarketingPixel),
    ("ads.linkedin.com", Category::MarketingPixel),
    ("analytics.tiktok.com", Category::MarketingPixel),
    ("ads.tiktok.com", Category::MarketingPixel),
    ("static.ads-twitter.com", Category::MarketingPixel),
    ("t.co", Category::MarketingPixel),
    ("bat.bing.com", Category::MarketingPixel),
    ("ct.pinterest.com", Category::MarketingPixel),
    ("s.pinimg.com", Category::MarketingPixel),
    // -- Consent / CMP
    ("onetrust.com", Category::ConsentCmp),
    ("cookielaw.org", Category::ConsentCmp),
    ("cdn.cookielaw.org", Category::ConsentCmp),
    ("cookiebot.com", Category::ConsentCmp),
    ("consent.cookiebot.com", Category::ConsentCmp),
    ("trustarc.com", Category::ConsentCmp),
    ("consent.trustarc.com", Category::ConsentCmp),
    ("ketchjs.com", Category::ConsentCmp),
    ("cdn.ketchjs.com", Category::ConsentCmp),
    ("cookiehub.com", Category::ConsentCmp),
    ("cdn.cookiehub.com", Category::ConsentCmp),
    ("usercentrics.eu", Category::ConsentCmp),
    ("app.usercentrics.eu", Category::ConsentCmp),
];

fn exact_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| ENTRIES.iter().map(|(p, _)| *p).collect())
}

/// Decide whether to block a URL. Returns Decision with category + matched
/// pattern when blocked.
pub fn decide(url: &str) -> Decision {
    let host = match url::Url::parse(url) {
        Ok(u) => u.host_str().map(|s| s.to_lowercase()),
        Err(_) => None,
    };
    let Some(host) = host else {
        return Decision::allow();
    };

    // Fast path: exact match.
    if let Some(&pattern) = exact_set().get(host.as_str()) {
        let cat = ENTRIES
            .iter()
            .find(|(p, _)| *p == pattern)
            .map(|(_, c)| *c)
            .unwrap_or(Category::Analytics);
        return Decision::block(cat, pattern);
    }

    // Suffix walk: host ends with `.{pattern}`.
    for (pattern, cat) in ENTRIES.iter() {
        if pattern.contains('/') {
            // Patterns like "facebook.com/tr" are path-bearing; check separately.
            if url.contains(pattern) {
                return Decision::block(*cat, pattern);
            }
            continue;
        }
        if host.ends_with(&format!(".{pattern}")) {
            return Decision::block(*cat, pattern);
        }
    }

    Decision::allow()
}

/// Number of entries in the blocklist. For diagnostics / `policy-check --info`.
pub fn entry_count() -> usize {
    ENTRIES.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_blocked(url: &str, expected_category: Category) {
        let d = decide(url);
        assert!(d.blocked, "expected block for {url}, got allow");
        assert_eq!(d.category, Some(expected_category), "wrong category for {url}");
    }

    fn assert_allowed(url: &str) {
        let d = decide(url);
        assert!(!d.blocked, "expected allow for {url}, got block ({:?})", d);
    }

    #[test]
    fn analytics_blocked() {
        assert_blocked("https://www.google-analytics.com/analytics.js", Category::Analytics);
        assert_blocked("https://cdn.amplitude.com/libs/amplitude-8.21.4-min.gz.js", Category::Analytics);
        assert_blocked("https://api.segment.io/v1/track", Category::Analytics);
        assert_blocked("https://heap.io/", Category::Analytics);
    }

    #[test]
    fn tag_managers_blocked() {
        assert_blocked("https://www.googletagmanager.com/gtm.js?id=GTM-XXXX", Category::TagManager);
        assert_blocked("https://assets.adobedtm.com/launch-XXXX.min.js", Category::TagManager);
    }

    #[test]
    fn ads_blocked() {
        assert_blocked("https://securepubads.g.doubleclick.net/tag/js/gpt.js", Category::Ads);
        assert_blocked("https://pub.doubleverify.com/dvbm.js", Category::Ads);
        assert_blocked("https://config.aps.amazon-adsystem.com/foo", Category::Ads);
        assert_blocked("https://cdn.concert.io/lib.js", Category::Ads);
    }

    #[test]
    fn session_replay_blocked() {
        assert_blocked("https://static.hotjar.com/c/hotjar-1234567.js", Category::SessionReplay);
        assert_blocked("https://cdn.logrocket.io/LogRocket.min.js", Category::SessionReplay);
    }

    #[test]
    fn marketing_pixels_blocked() {
        assert_blocked("https://connect.facebook.net/en_US/fbevents.js", Category::MarketingPixel);
        assert_blocked("https://www.facebook.com/tr?id=12345", Category::MarketingPixel);
        assert_blocked("https://snap.licdn.com/li.lms-analytics/insight.min.js", Category::MarketingPixel);
        assert_blocked("https://bat.bing.com/bat.js", Category::MarketingPixel);
    }

    #[test]
    fn consent_cmp_blocked() {
        assert_blocked("https://cdn.cookielaw.org/scripttemplates/otSDKStub.js", Category::ConsentCmp);
        assert_blocked("https://cdn.ketchjs.com/plugins/v1/tcf/stub.js", Category::ConsentCmp);
    }

    #[test]
    fn error_beacons_blocked() {
        assert_blocked("https://browser.sentry-cdn.com/7.0.0/bundle.min.js", Category::ErrorBeacon);
        assert_blocked("https://js-agent.newrelic.com/nr-1234.min.js", Category::ErrorBeacon);
    }

    #[test]
    fn clean_sites_allowed() {
        // From scripts/policy_baseline.py runs against real sites:
        assert_allowed("https://news.ycombinator.com/");
        assert_allowed("https://news.ycombinator.com/news.js?abc");
        assert_allowed("https://en.wikipedia.org/wiki/Bayesian_inference");
        assert_allowed("https://upload.wikimedia.org/wikipedia/commons/x.svg");
        // First-party app code on commercial sites should NOT be blocked:
        assert_allowed("https://i.forbesimg.com/simple-site/_next/static/chunks/main-abc.js");
        assert_allowed("https://www.cnbc.com/some-page");
        assert_allowed("https://zephr-templates.cnbc.com/foo");
        assert_allowed("https://assets.zephr.com/foo");
    }

    #[test]
    fn malformed_urls_allowed() {
        assert_allowed("");
        assert_allowed("not a url");
        assert_allowed("javascript:void(0)");
        assert_allowed("data:text/plain,foo");
    }

    #[test]
    fn case_insensitive_hostname() {
        let d = decide("https://WWW.GOOGLE-ANALYTICS.COM/foo");
        assert!(d.blocked);
    }

    #[test]
    fn stealth_safety_no_fingerprinting_hosts() {
        // These hosts MUST be allowed: blocking them is itself a bot signal
        // because the page expects the script to load and produce side
        // effects. See spec §8.2 and the module-level STEALTH SAFETY note.
        let must_allow = &[
            // FingerprintJS
            "https://fpjs.io/v3/abc.js",
            "https://fpcdn.io/v3/abc.js",
            "https://api.fpjs.io/foo",
            "https://fingerprint.com/foo",
            // PerimeterX
            "https://client.perimeterx.net/foo",
            "https://captcha.px-cdn.net/foo",
            // Datadome
            "https://js.datado.me/foo",
            "https://geo.captcha-delivery.com/foo",
            "https://api-js.datadome.co/foo",
            // Akamai BMP
            "https://www.akamaihd.net/foo",
            // Cloudflare challenges
            "https://challenges.cloudflare.com/turnstile/v0/api.js",
            "https://cdn.cloudflare.com/foo",
            // hCaptcha / reCAPTCHA
            "https://hcaptcha.com/1/api.js",
            "https://www.google.com/recaptcha/api.js",
            "https://www.gstatic.com/recaptcha/foo.js",
            // Arkose Labs
            "https://client-api.arkoselabs.com/foo",
            // Imperva
            "https://www.imperva.com/foo",
        ];
        for url in must_allow {
            let d = decide(url);
            assert!(
                !d.blocked,
                "STEALTH REGRESSION: {url} got blocked ({:?}). \
                 Anti-bot/fingerprinting hosts must not be blocked — their \
                 absence is itself a detection signal. See module docs.",
                d
            );
        }
    }

    #[test]
    fn entries_are_sane() {
        // No empty patterns, no duplicates, all-lowercase.
        let mut seen = HashSet::new();
        for (p, _) in ENTRIES {
            assert!(!p.is_empty(), "empty pattern");
            assert_eq!(p.to_lowercase(), *p, "non-lowercase pattern: {p}");
            assert!(seen.insert(*p), "duplicate pattern: {p}");
        }
    }
}
