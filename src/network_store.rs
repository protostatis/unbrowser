//! Network response capture for content extraction.
//!
//! Many SPAs (Zillow listings, npm package metadata, Reddit JSON, GraphQL-
//! backed apps) keep their content in API responses and only assemble it
//! into the DOM via hydration. For an LLM agent doing extraction, the
//! API response is often *cleaner* than the rendered DOM — less template
//! noise, structured shape, no need to wait for hydration to complete.
//!
//! This module captures every fetch/XHR response that looks content-bearing
//! (JSON / GraphQL / NDJSON / Next/Nuxt route data), ranks them by likely
//! content value, and surfaces them via the navigate result and the
//! `network_stores` RPC method.
//!
//! Storage is bounded: a sliding window of the most recent captures, capped
//! by both entry count and total bytes. Per-capture body is truncated.
//!
//! Hooks: `run_fetch` in main.rs calls `maybe_capture` after every fetch
//! completes (worker thread). The store is shared via Arc<Mutex<>> with the
//! Session.

use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_MAX_ENTRIES: usize = 100;
const DEFAULT_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024; // 16 MB (bumped from 4 MB)
// Bumped from 64 KB to 256 KB. Most JSON-API responses (paginated listings,
// graphql queries) fit within this and parse cleanly; truncated bodies
// previously could produce invalid JSON for Zillow-style listings or large
// graph queries. (PR #7 review medium.)
const DEFAULT_MAX_BODY_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct NetworkCapture {
    /// Stable per-process auto-incrementing id. Forward-compat hook for a
    /// future `network_store_get {capture_id}` that returns full body if
    /// we ever store more than the preview. (PR #7 review medium.)
    pub capture_id: u64,
    pub url: String,
    pub method: String,
    pub status: u16,
    pub content_type: String,
    /// Original full body length, before truncation.
    pub body_bytes: usize,
    /// True when `body_preview` is truncated; consumers must NOT assume
    /// the preview parses as valid JSON in that case.
    pub body_truncated: bool,
    /// Truncated preview of the body, up to `max_body_bytes`. Renamed from
    /// `body` (PR #7 review medium) so the API surface makes truncation
    /// explicit; full retrieval is not currently supported (we don't store
    /// more than the preview).
    pub body_preview: String,
    pub captured_at_ms: u64,
    pub score: u32,
    pub kind: ContentKind,
    /// nav_id of the navigation that was in flight when this fetch
    /// resolved. None for fetches issued outside any navigation. The
    /// navigate result and the network_stores RPC default to filtering
    /// on the current/most-recent navigation_id so page A captures don't
    /// leak into page B's summary. (PR #7 review medium.)
    pub navigation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Json,
    GraphQl,
    Ndjson,
    JsonLd,
    NextRouteData,
    NuxtRouteData,
    /// Capture-eligible by URL/body shape but not a recognized format.
    JsonLikely,
}

pub struct NetworkStore {
    captures: VecDeque<NetworkCapture>,
    current_bytes: usize,
    next_capture_id: u64,
    pub max_entries: usize,
    pub max_total_bytes: usize,
    pub max_body_bytes: usize,
}

impl Default for NetworkStore {
    fn default() -> Self {
        Self {
            captures: VecDeque::new(),
            current_bytes: 0,
            next_capture_id: 1,
            max_entries: DEFAULT_MAX_ENTRIES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

/// Filter for ranked() / summary(): which navigation_id(s) to consider.
#[derive(Debug, Clone)]
pub enum NavScope<'a> {
    /// Only captures bound to this navigation_id (None matches captures with
    /// no nav binding).
    Only(&'a str),
    /// All captures regardless of navigation_id.
    All,
}

impl NetworkStore {
    /// Inspect a completed fetch response and capture if content-bearing.
    /// `navigation_id` is the in-flight navigation when the fetch resolved
    /// (None for fetches issued outside any navigate call).
    /// Returns true if captured.
    pub fn maybe_capture(
        &mut self,
        url: &str,
        method: &str,
        status: u16,
        headers: &HashMap<String, String>,
        body: &str,
        navigation_id: Option<&str>,
    ) -> bool {
        // Only successful responses; non-2xx is typically auth/redirect
        // noise, not content.
        if !(200..300).contains(&status) {
            return false;
        }
        if body.is_empty() {
            return false;
        }
        let content_type = headers
            .get("content-type")
            .cloned()
            .unwrap_or_default()
            .to_lowercase();

        let (score, kind) = classify(&content_type, url, body);
        if score == 0 {
            return false;
        }

        let body_bytes = body.len();
        let body_truncated = body_bytes > self.max_body_bytes;
        let stored_body = if body_truncated {
            // Truncate at a character boundary to keep the JSON parseable
            // when possible (best-effort — caller must check `body_truncated`).
            let mut end = self.max_body_bytes;
            while end > 0 && !body.is_char_boundary(end) {
                end -= 1;
            }
            body[..end].to_string()
        } else {
            body.to_string()
        };

        // Evict oldest until we fit.
        while self.captures.len() >= self.max_entries
            || self.current_bytes + stored_body.len() > self.max_total_bytes
        {
            if let Some(old) = self.captures.pop_front() {
                self.current_bytes = self.current_bytes.saturating_sub(old.body_preview.len());
            } else {
                break;
            }
        }

        self.current_bytes += stored_body.len();
        let capture_id = self.next_capture_id;
        self.next_capture_id += 1;
        self.captures.push_back(NetworkCapture {
            capture_id,
            url: url.to_string(),
            method: method.to_string(),
            status,
            content_type,
            body_bytes,
            body_truncated,
            body_preview: stored_body,
            captured_at_ms: now_ms(),
            score,
            kind,
            navigation_id: navigation_id.map(String::from),
        });
        true
    }

    /// Top N captures by score, optionally filtered by host substring and
    /// navigation scope. Returned in score-descending order. `body_preview`
    /// is preserved on each entry.
    pub fn ranked(
        &self,
        limit: usize,
        host_filter: Option<&str>,
        nav_scope: NavScope,
    ) -> Vec<NetworkCapture> {
        let mut v: Vec<NetworkCapture> = self
            .captures
            .iter()
            .filter(|c| match host_filter {
                None => true,
                Some(h) => host_of(&c.url).contains(h),
            })
            .filter(|c| match &nav_scope {
                NavScope::All => true,
                NavScope::Only(id) => c.navigation_id.as_deref() == Some(id),
            })
            .cloned()
            .collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.score));
        v.truncate(limit);
        v
    }

    /// Quick summary for embedding in navigate result without dumping bodies.
    /// `nav_scope` defaults to filtering by current nav_id when called from
    /// navigate_with — page B never sees page A's captures in its summary.
    pub fn summary(&self, top_k: usize, nav_scope: NavScope) -> NetworkStoreSummary {
        let scoped: Vec<&NetworkCapture> = self
            .captures
            .iter()
            .filter(|c| match &nav_scope {
                NavScope::All => true,
                NavScope::Only(id) => c.navigation_id.as_deref() == Some(id),
            })
            .collect();
        let mut tops = scoped.clone();
        tops.sort_by_key(|c| std::cmp::Reverse(c.score));
        tops.truncate(top_k);
        NetworkStoreSummary {
            count: scoped.len(),
            total_bytes: scoped.iter().map(|c| c.body_preview.len()).sum(),
            top: tops
                .iter()
                .map(|c| NetworkCaptureMeta {
                    capture_id: c.capture_id,
                    url: c.url.clone(),
                    status: c.status,
                    content_type: c.content_type.clone(),
                    body_bytes: c.body_bytes,
                    body_truncated: c.body_truncated,
                    score: c.score,
                    kind: c.kind,
                    navigation_id: c.navigation_id.clone(),
                })
                .collect(),
        }
    }

    pub fn clear(&mut self) {
        self.captures.clear();
        self.current_bytes = 0;
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.captures.len()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkStoreSummary {
    pub count: usize,
    pub total_bytes: usize,
    pub top: Vec<NetworkCaptureMeta>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkCaptureMeta {
    pub capture_id: u64,
    pub url: String,
    pub status: u16,
    pub content_type: String,
    pub body_bytes: usize,
    pub body_truncated: bool,
    pub score: u32,
    pub kind: ContentKind,
    pub navigation_id: Option<String>,
}

/// Heuristic ranking. Score 0 means "skip this response."
/// Combined Content-Type + URL pattern + body shape signals.
fn classify(content_type: &str, url: &str, body: &str) -> (u32, ContentKind) {
    let ct = content_type;

    // Hard skip: media, fonts, css, html, plain js. These never carry the
    // structured data we want to surface; capturing them just wastes the
    // store budget.
    if ct.starts_with("image/")
        || ct.starts_with("font/")
        || ct.starts_with("video/")
        || ct.starts_with("audio/")
    {
        return (0, ContentKind::JsonLikely);
    }
    if ct.contains("text/css")
        || ct.contains("text/html")
        || (ct.contains("javascript") && !ct.contains("json"))
    {
        return (0, ContentKind::JsonLikely);
    }

    let url_lower = url.to_lowercase();
    let mut score: u32 = 0;
    let mut kind = ContentKind::JsonLikely;

    // ---- Content-Type signals (strongest) ----
    if ct.contains("application/graphql") || ct.contains("graphql+json") {
        score += 40;
        kind = ContentKind::GraphQl;
    } else if ct.contains("application/ld+json") {
        score += 25;
        kind = ContentKind::JsonLd;
    } else if ct.contains("application/x-ndjson") || ct.contains("application/jsonl") {
        score += 25;
        kind = ContentKind::Ndjson;
    } else if ct.contains("application/json") || ct.contains("+json") {
        score += 30;
        kind = ContentKind::Json;
    }

    // ---- URL pattern signals ----
    if url_lower.contains("/graphql") || url_lower.contains("/gql") {
        score += 25;
        if matches!(kind, ContentKind::JsonLikely | ContentKind::Json) {
            kind = ContentKind::GraphQl;
        }
    }
    if url_lower.contains("/_next/data/") || url_lower.contains("__nextjs__") {
        score += 30;
        kind = ContentKind::NextRouteData;
    }
    if url_lower.contains("/__nuxt") || url_lower.contains("/nuxt/") {
        score += 25;
        kind = ContentKind::NuxtRouteData;
    }
    if url_lower.contains("/api/")
        || url_lower.contains("/v1/")
        || url_lower.contains("/v2/")
        || url_lower.contains("/v3/")
    {
        score += 15;
    }

    // ---- Body shape signals (always cheap to try) ----
    let trimmed = body.trim_start();
    let looks_jsonish = trimmed.starts_with('{') || trimmed.starts_with('[');
    if looks_jsonish {
        // Cheap parse check on first 4 KB — if it parses, it's structured
        // data. Don't parse the whole thing (could be MB).
        let probe = if body.len() > 4096 {
            // Find a char boundary near 4096 for the slice
            let mut end = 4096;
            while end > 0 && !body.is_char_boundary(end) {
                end -= 1;
            }
            &body[..end]
        } else {
            body
        };
        // Probe parses to a `Value` if it's complete enough. For truncated
        // probes we can't fully parse, but a starts-with-`{` body that
        // *would* parse if complete is still strong evidence.
        if serde_json::from_str::<serde_json::Value>(probe).is_ok() {
            score += 15;
        } else if body.len() > 4096 && serde_json::from_str::<serde_json::Value>(body).is_ok() {
            // Whole-body parse for medium bodies (parser stops at end of value;
            // bounded by O(body_size)).
            score += 15;
        } else {
            // Looks JSONy but didn't parse — still a weak positive signal,
            // small bonus for the open brace/bracket.
            score += 5;
        }

        // Size bonus — bigger structured payloads are more likely to be
        // the real data.
        if body.len() > 2_000 {
            score += 5;
        }
        if body.len() > 20_000 {
            score += 5;
        }
    }

    // Threshold — must look at least somewhat content-bearing to capture.
    if score < 25 {
        return (0, kind);
    }
    (score, kind)
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_lowercase()))
        .unwrap_or_default()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(ct: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("content-type".to_string(), ct.to_string());
        m
    }

    fn cap(s: &mut NetworkStore, url: &str, ct: &str, body: &str) -> bool {
        s.maybe_capture(url, "GET", 200, &h(ct), body, None)
    }
    fn cap_nav(s: &mut NetworkStore, url: &str, ct: &str, body: &str, nav: &str) -> bool {
        s.maybe_capture(url, "GET", 200, &h(ct), body, Some(nav))
    }

    #[test]
    fn captures_application_json() {
        let mut s = NetworkStore::default();
        let body = r#"{"data": {"items": [1,2,3], "total": 42}}"#;
        assert!(cap(
            &mut s,
            "https://api.example.com/v1/items",
            "application/json",
            body
        ));
        assert_eq!(s.len(), 1);
        let r = &s.ranked(10, None, NavScope::All)[0];
        assert_eq!(r.kind, ContentKind::Json);
        assert!(r.score >= 30);
        assert_eq!(r.capture_id, 1);
    }

    #[test]
    fn captures_graphql() {
        let mut s = NetworkStore::default();
        assert!(s.maybe_capture(
            "https://api.example.com/graphql",
            "POST",
            200,
            &h("application/graphql+json"),
            r#"{"data":{"viewer":{"id":"x"}}}"#,
            None,
        ));
        let r = &s.ranked(10, None, NavScope::All)[0];
        assert_eq!(r.kind, ContentKind::GraphQl);
    }

    #[test]
    fn captures_next_route_data() {
        let mut s = NetworkStore::default();
        assert!(cap(
            &mut s,
            "https://example.com/_next/data/abc/page.json",
            "application/json",
            r#"{"pageProps":{"data":[1,2,3]}}"#
        ));
        let r = &s.ranked(10, None, NavScope::All)[0];
        assert_eq!(r.kind, ContentKind::NextRouteData);
    }

    #[test]
    fn skips_html() {
        let mut s = NetworkStore::default();
        assert!(!cap(
            &mut s,
            "https://example.com/",
            "text/html",
            "<html>x</html>"
        ));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn skips_image_css_js() {
        let mut s = NetworkStore::default();
        assert!(!cap(
            &mut s,
            "https://example.com/x.png",
            "image/png",
            "binary"
        ));
        assert!(!cap(
            &mut s,
            "https://example.com/x.css",
            "text/css",
            "body{}"
        ));
        assert!(!cap(
            &mut s,
            "https://example.com/x.js",
            "application/javascript",
            "var x=1"
        ));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn skips_non_2xx() {
        let mut s = NetworkStore::default();
        for status in [401u16, 500, 302] {
            assert!(!s.maybe_capture(
                "https://api.example.com/v1/x",
                "GET",
                status,
                &h("application/json"),
                r#"{"error":"x"}"#,
                None,
            ));
        }
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn skips_empty_body() {
        let mut s = NetworkStore::default();
        assert!(!cap(
            &mut s,
            "https://api.example.com/v1/x",
            "application/json",
            ""
        ));
    }

    #[test]
    fn truncates_large_body() {
        let mut s = NetworkStore::default();
        let big = "{".to_string() + &"\"k\":\"v\",".repeat(60_000) + "\"end\":1}";
        assert!(cap(
            &mut s,
            "https://api.example.com/v1/x",
            "application/json",
            &big
        ));
        let r = &s.ranked(10, None, NavScope::All)[0];
        assert!(r.body_truncated);
        assert_eq!(r.body_bytes, big.len());
        assert!(r.body_preview.len() <= s.max_body_bytes);
    }

    #[test]
    fn ranks_by_score() {
        let mut s = NetworkStore::default();
        cap(
            &mut s,
            "https://example.com/data.json",
            "application/json",
            r#"{"a":1}"#,
        );
        cap(
            &mut s,
            "https://example.com/graphql",
            "application/graphql+json",
            r#"{"data":{"x":[1,2,3]}}"#,
        );
        let r = s.ranked(10, None, NavScope::All);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].kind, ContentKind::GraphQl);
    }

    #[test]
    fn evicts_when_over_capacity() {
        let mut s = NetworkStore {
            max_entries: 3,
            ..NetworkStore::default()
        };
        for i in 0..5 {
            let url = format!("https://api.example.com/v1/item/{i}");
            cap(
                &mut s,
                &url,
                "application/json",
                &format!(r#"{{"id":{i}}}"#),
            );
        }
        assert_eq!(s.len(), 3);
        let urls: Vec<_> = s
            .ranked(10, None, NavScope::All)
            .into_iter()
            .map(|c| c.url)
            .collect();
        assert!(urls.iter().any(|u| u.ends_with("/2")));
        assert!(urls.iter().any(|u| u.ends_with("/4")));
        assert!(!urls.iter().any(|u| u.ends_with("/0")));
    }

    #[test]
    fn host_filter_works() {
        let mut s = NetworkStore::default();
        cap(
            &mut s,
            "https://api.first.com/v1/x",
            "application/json",
            r#"{"a":1}"#,
        );
        cap(
            &mut s,
            "https://api.second.com/v1/x",
            "application/json",
            r#"{"b":2}"#,
        );
        let r = s.ranked(10, Some("first"), NavScope::All);
        assert_eq!(r.len(), 1);
        assert!(r[0].url.contains("first.com"));
    }

    #[test]
    fn capture_ids_monotonic() {
        let mut s = NetworkStore::default();
        for i in 0..3 {
            cap(
                &mut s,
                &format!("https://api.example.com/v1/{i}"),
                "application/json",
                r#"{"a":1}"#,
            );
        }
        let r = s.ranked(10, None, NavScope::All);
        let mut ids: Vec<u64> = r.iter().map(|c| c.capture_id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn nav_scope_filters_correctly() {
        // Page A's captures shouldn't surface in page B's summary. (PR #7 review.)
        let mut s = NetworkStore::default();
        cap_nav(
            &mut s,
            "https://api.a.com/x",
            "application/json",
            r#"{"a":1}"#,
            "nav_1",
        );
        cap_nav(
            &mut s,
            "https://api.a.com/y",
            "application/json",
            r#"{"a":2}"#,
            "nav_1",
        );
        cap_nav(
            &mut s,
            "https://api.b.com/z",
            "application/json",
            r#"{"b":1}"#,
            "nav_2",
        );

        let r1 = s.ranked(10, None, NavScope::Only("nav_1"));
        assert_eq!(r1.len(), 2);
        assert!(r1.iter().all(|c| c.url.contains("a.com")));

        let r2 = s.ranked(10, None, NavScope::Only("nav_2"));
        assert_eq!(r2.len(), 1);
        assert!(r2[0].url.contains("b.com"));

        let all = s.ranked(10, None, NavScope::All);
        assert_eq!(all.len(), 3);

        // Summary scoping
        let sum1 = s.summary(5, NavScope::Only("nav_1"));
        assert_eq!(sum1.count, 2);
        let sum2 = s.summary(5, NavScope::Only("nav_2"));
        assert_eq!(sum2.count, 1);
        // Top-K within scope: nav_2 summary should only contain nav_2's capture
        assert!(
            sum2.top
                .iter()
                .all(|c| c.navigation_id.as_deref() == Some("nav_2"))
        );
    }

    #[test]
    fn nav_scope_only_excludes_unbound() {
        // Captures with no nav_id (driver-initiated fetch) shouldn't pollute
        // an explicit Only("nav_X") query.
        let mut s = NetworkStore::default();
        cap(
            &mut s,
            "https://api.x.com/a",
            "application/json",
            r#"{"x":1}"#,
        );
        cap_nav(
            &mut s,
            "https://api.x.com/b",
            "application/json",
            r#"{"x":2}"#,
            "nav_1",
        );
        let r = s.ranked(10, None, NavScope::Only("nav_1"));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://api.x.com/b");
    }
}
