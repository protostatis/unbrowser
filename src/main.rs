use std::io::Write;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow};
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use rquickjs::FromJs;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};

const DOM_JS: &str = include_str!("js/dom.js");
const SHIMS_JS: &str = include_str!("js/shims.js");
const BLOCKMAP_JS: &str = include_str!("js/blockmap.js");
const INTERACT_JS: &str = include_str!("js/interact.js");

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct Response {
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

#[derive(Default)]
struct CookieJar {
    inner: RwLock<cookie_store::CookieStore>,
}

impl rquest::cookie::CookieStore for CookieJar {
    fn set_cookies(&self, url: &url::Url, headers: &mut dyn Iterator<Item = &http::HeaderValue>) {
        let parsed: Vec<cookie::Cookie<'static>> = headers
            .filter_map(|h| h.to_str().ok())
            .filter_map(|s| cookie::Cookie::parse(s.to_string()).ok())
            .collect();
        if let Ok(mut store) = self.inner.write() {
            store.store_response_cookies(parsed.into_iter(), url);
        }
    }

    fn cookies(&self, url: &url::Url) -> Option<http::HeaderValue> {
        let store = self.inner.read().ok()?;
        let s: String = store
            .get_request_values(url)
            .map(|(n, v)| format!("{n}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        if s.is_empty() {
            None
        } else {
            http::HeaderValue::from_str(&s).ok()
        }
    }
}

impl CookieJar {
    fn export(&self) -> Vec<Value> {
        match self.inner.read() {
            Ok(s) => s
                .iter_unexpired()
                .map(|c| {
                    json!({
                        "name": c.name(),
                        "value": c.value(),
                        "domain": c.domain(),
                        "path": c.path(),
                        "secure": c.secure().unwrap_or(false),
                        "http_only": c.http_only().unwrap_or(false),
                    })
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn import(&self, items: &[Value], default_url: Option<&str>) -> Result<usize> {
        let mut store = self
            .inner
            .write()
            .map_err(|_| anyhow!("cookie jar lock poisoned"))?;
        let mut added = 0;
        for item in items {
            let (cookie_str, url_str) = build_cookie(item, default_url)?;
            let url = url::Url::parse(&url_str).map_err(|e| anyhow!("parse url: {e}"))?;
            if let Ok(c) = cookie::Cookie::parse(cookie_str) {
                store.store_response_cookies(std::iter::once(c.into_owned()), &url);
                added += 1;
            }
        }
        Ok(added)
    }

    fn clear(&self) {
        if let Ok(mut s) = self.inner.write() {
            s.clear();
        }
    }
}

// Accept either a Set-Cookie string or a {name, value, domain, path?, secure?, http_only?, url?} object.
fn build_cookie(item: &Value, default_url: Option<&str>) -> Result<(String, String)> {
    if let Some(s) = item.as_str() {
        // Bare Set-Cookie string — derive url from default_url
        let url = default_url
            .map(String::from)
            .ok_or_else(|| anyhow!("string-form cookie requires 'url' param"))?;
        return Ok((s.to_string(), url));
    }
    let obj = item
        .as_object()
        .ok_or_else(|| anyhow!("cookie must be string or object"))?;
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("cookie missing 'name'"))?;
    let value = obj.get("value").and_then(|v| v.as_str()).unwrap_or("");
    let domain = obj.get("domain").and_then(|v| v.as_str());
    let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("/");
    let secure = obj.get("secure").and_then(|v| v.as_bool()).unwrap_or(false);
    let http_only = obj
        .get("http_only")
        .or_else(|| obj.get("httpOnly"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut s = format!("{name}={value}; Path={path}");
    if let Some(d) = domain {
        s.push_str(&format!("; Domain={d}"));
    }
    if secure {
        s.push_str("; Secure");
    }
    if http_only {
        s.push_str("; HttpOnly");
    }

    let url = obj
        .get("url")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            domain.map(|d| {
                let host = d.trim_start_matches('.');
                let scheme = if secure { "https" } else { "http" };
                format!("{scheme}://{host}/")
            })
        })
        .or_else(|| default_url.map(String::from))
        .ok_or_else(|| anyhow!("cookie {name} has no 'url' or 'domain'"))?;

    Ok((s, url))
}

struct Session {
    // Holds the QuickJS runtime alive for the Context's lifetime.
    _js_rt: rquickjs::Runtime,
    js_ctx: rquickjs::Context,
    http: rquest::Client,
    jar: Arc<CookieJar>,
    last_url: Option<String>,
    last_body: Option<String>,
}

impl Session {
    fn new() -> Result<Self> {
        let js_rt = rquickjs::Runtime::new().context("rquickjs Runtime::new")?;
        let js_ctx = rquickjs::Context::full(&js_rt).context("rquickjs Context::full")?;
        let jar = Arc::new(CookieJar::default());
        let http = rquest::Client::builder()
            .emulation(rquest_util::Emulation::Chrome131)
            .cookie_provider(jar.clone())
            // .emulation(...) appears to clobber the default redirect policy.
            // Explicit follow-up-to-10 matches Chrome's behavior on http://github.com,
            // httpbin.org/redirect/N, and the Yahoo "sad panda" 301 chain.
            .redirect(rquest::redirect::Policy::limited(10))
            .build()
            .context("rquest client build")?;
        // Install JS layers in order:
        //   1. dom.js     — document, Element, querySelector, __seedDOM, etc.
        //   2. shims.js   — passive browser globals (window, navigator, location,
        //                   storage, etc.) — coherent with our Chrome 131 TLS FP
        //   3. blockmap.js — __blockmap() page-summary walker
        //   4. interact.js — __click, __type, __byRef, __formData
        js_ctx.with(|ctx| -> Result<()> {
            ctx.eval::<(), _>(DOM_JS)
                .map_err(|e| anyhow!("eval dom.js: {e}"))?;
            ctx.eval::<(), _>(SHIMS_JS)
                .map_err(|e| anyhow!("eval shims.js: {e}"))?;
            ctx.eval::<(), _>(BLOCKMAP_JS)
                .map_err(|e| anyhow!("eval blockmap.js: {e}"))?;
            ctx.eval::<(), _>(INTERACT_JS)
                .map_err(|e| anyhow!("eval interact.js: {e}"))?;
            Ok(())
        })?;
        Ok(Self {
            _js_rt: js_rt,
            js_ctx,
            http,
            jar,
            last_url: None,
            last_body: None,
        })
    }

    fn eval(&self, code: &str) -> Result<Value> {
        self.js_ctx.with(|ctx| -> Result<Value> {
            let val = match ctx.eval::<rquickjs::Value, _>(code) {
                Ok(v) => v,
                Err(rquickjs::Error::Exception) => {
                    return Err(anyhow!("{}", format_js_exception(ctx.catch())));
                }
                Err(e) => return Err(anyhow!("js eval: {e}")),
            };
            if val.is_undefined() {
                return Ok(Value::Null);
            }
            let json_obj: rquickjs::Object = ctx
                .globals()
                .get("JSON")
                .map_err(|e| anyhow!("get JSON: {e}"))?;
            let stringify: rquickjs::Function = json_obj
                .get("stringify")
                .map_err(|e| anyhow!("get stringify: {e}"))?;
            let result: rquickjs::Value = stringify
                .call((val,))
                .map_err(|e| anyhow!("call stringify: {e}"))?;
            if result.is_undefined() || result.is_null() {
                return Ok(Value::Null);
            }
            let s = String::from_js(&ctx, result).map_err(|e| anyhow!("to string: {e}"))?;
            Ok(serde_json::from_str(&s).unwrap_or(Value::String(s)))
        })
    }

    async fn navigate(&mut self, url: &str) -> Result<Value> {
        let resp = self.http.get(url).send().await.context("http get")?;
        let status = resp.status().as_u16();
        let final_url = resp.url().to_string();

        // Snapshot useful response headers before consuming the response body.
        // Multi-value headers (Set-Cookie) are joined with ' || ' since they're
        // mostly diagnostic — the actual cookie storage already happened in
        // rquest's CookieStore impl.
        let mut headers: serde_json::Map<String, Value> = serde_json::Map::new();
        for (name, value) in resp.headers() {
            let key = name.as_str().to_lowercase();
            let v = value.to_str().unwrap_or("").to_string();
            match headers.get_mut(&key) {
                Some(Value::String(existing)) => {
                    *existing = format!("{existing} || {v}");
                }
                _ => {
                    headers.insert(key, Value::String(v));
                }
            }
        }

        let body = resp.text().await.context("read body")?;
        let bytes = body.len();

        let challenge = detect_challenge(status, &body);

        let tree = parse_html_to_tree(&body);
        self.seed_dom(&tree)?;

        // Update window.location for any page scripts that read it.
        let url_lit = serde_json::to_string(&final_url)?;
        let _ = self.eval(&format!("__setLocation({url_lit})"));

        self.last_url = Some(final_url.clone());
        self.last_body = Some(body);

        let blockmap = self.blockmap().unwrap_or(Value::Null);

        Ok(json!({
            "status": status,
            "url": final_url,
            "bytes": bytes,
            "headers": Value::Object(headers),
            "blockmap": blockmap,
            "challenge": challenge,
        }))
    }

    fn blockmap(&self) -> Result<Value> {
        self.eval("__blockmap()")
    }

    fn seed_dom(&self, tree: &Value) -> Result<()> {
        let tree_str = serde_json::to_string(tree)?;
        // Embed the JSON string as a JS string literal (double-encode to escape safely).
        let js_literal = serde_json::to_string(&tree_str)?;
        let code = format!("__seedDOM(JSON.parse({js_literal}))");
        self.js_ctx.with(|ctx| -> Result<()> {
            ctx.eval::<(), _>(code)
                .map_err(|e| anyhow!("seed dom: {e}"))?;
            Ok(())
        })
    }

    fn query(&self, selector: &str) -> Result<Value> {
        let sel_lit = serde_json::to_string(selector)?;
        let code = format!(
            "(function(){{ \
                var els = document.querySelectorAll({sel_lit}); \
                return els.map(function(e){{ \
                    return {{ \
                        ref: 'e:' + e._id, \
                        tag: e.tagName.toLowerCase(), \
                        attrs: e._attributes, \
                        text: (e.textContent || '').trim().slice(0, 200) \
                    }}; \
                }}); \
            }})()"
        );
        self.eval(&code)
    }

    fn text(&self, selector: &str) -> Result<Value> {
        let sel_lit = serde_json::to_string(selector)?;
        let code = format!(
            "(function(){{ \
                var el = document.querySelector({sel_lit}); \
                return el ? (el.textContent || '').trim() : null; \
            }})()"
        );
        self.eval(&code)
    }

    // Returns the textContent of the page's main content area, excluding chrome
    // (header, nav, footer, aside, script, style) — recursively, so even
    // chrome nested INSIDE <main> (e.g. Wikipedia's table-of-contents <nav>)
    // is skipped.
    //
    // Strategy:
    //  1. <main> or [role=main] if present (walk inside, skip chrome)
    //  2. exactly one <article>
    //  3. fallback: the whole body with chrome subtrees stripped
    fn text_main(&self) -> Result<Value> {
        let code = r#"(function(){
            function clean(s){ return (s || '').replace(/\s+/g, ' ').trim(); }
            // Walk subtree, concatenate text, skipping chrome tags.
            function nonChromeText(root){
                var out = [];
                (function walk(node){
                    if (!node) return;
                    if (node.nodeType === 3) {
                        out.push(node.textContent);
                        return;
                    }
                    if (node.nodeType !== 1) return;
                    var t = (node.tagName || '').toLowerCase();
                    if (t === 'script' || t === 'style' ||
                        t === 'header' || t === 'nav' ||
                        t === 'footer' || t === 'aside' ||
                        t === 'noscript') return;
                    for (var i = 0; i < node.childNodes.length; i++) walk(node.childNodes[i]);
                })(root);
                return clean(out.join(' '));
            }

            var main = document.querySelector('main, [role="main"]');
            if (main) {
                var t = nonChromeText(main);
                if (t.length > 0) return t;
            }
            var articles = document.querySelectorAll('article');
            if (articles.length === 1) {
                var t = nonChromeText(articles[0]);
                if (t.length > 0) return t;
            }
            return nonChromeText(document.body);
        })()"#;
        self.eval(code)
    }

    async fn click(&mut self, ref_: &str) -> Result<Value> {
        let lit = serde_json::to_string(ref_)?;
        let result = self.eval(&format!("__click({lit})"))?;
        if let Some(false) = result.get("ok").and_then(|v| v.as_bool()) {
            return Ok(result);
        }
        // Auto-follow <a href> clicks unless preventDefault'd (which sets follow=null).
        let follow = result.get("follow").and_then(|v| v.as_str()).unwrap_or("");
        if !follow.is_empty() {
            let target = self.resolve_url(follow)?;
            return self.navigate(&target).await;
        }
        Ok(result)
    }

    fn type_(&self, ref_: &str, text: &str) -> Result<Value> {
        let r = serde_json::to_string(ref_)?;
        let t = serde_json::to_string(text)?;
        self.eval(&format!("__type({r}, {t})"))
    }

    async fn submit(&mut self, ref_: &str) -> Result<Value> {
        let lit = serde_json::to_string(ref_)?;
        let info = self.eval(&format!("__formData({lit})"))?;
        if let Some(false) = info.get("ok").and_then(|v| v.as_bool()) {
            return Ok(info);
        }
        let action = info.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let method = info.get("method").and_then(|v| v.as_str()).unwrap_or("get");
        if method != "get" {
            return Err(anyhow!(
                "only GET form submission supported in v1, form method is '{method}'"
            ));
        }
        let pairs: Vec<(String, String)> = info
            .get("fields")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|f| {
                let arr = f.as_array()?;
                if arr.len() != 2 {
                    return None;
                }
                Some((arr[0].as_str()?.to_string(), arr[1].as_str()?.to_string()))
            })
            .collect();

        let mut target = url::Url::parse(&self.resolve_url(action)?)
            .map_err(|e| anyhow!("resolve action url: {e}"))?;
        {
            let mut qp = target.query_pairs_mut();
            qp.clear();
            for (n, v) in &pairs {
                qp.append_pair(n, v);
            }
        }
        self.navigate(target.as_str()).await
    }

    fn resolve_url(&self, href: &str) -> Result<String> {
        if href.is_empty() {
            return self
                .last_url
                .clone()
                .ok_or_else(|| anyhow!("no current page — call navigate first"));
        }
        if let Ok(u) = url::Url::parse(href)
            && u.has_host()
        {
            return Ok(u.to_string());
        }
        let base = self
            .last_url
            .as_deref()
            .ok_or_else(|| anyhow!("no current page — call navigate first"))?;
        let base_url = url::Url::parse(base).map_err(|e| anyhow!("base url: {e}"))?;
        Ok(base_url
            .join(href)
            .map_err(|e| anyhow!("join '{href}': {e}"))?
            .to_string())
    }
}

fn format_js_exception(ex: rquickjs::Value) -> String {
    if let Some(obj) = ex.as_object() {
        let name: String = obj.get("name").unwrap_or_else(|_| "Error".to_string());
        let msg: String = obj.get("message").unwrap_or_default();
        if !msg.is_empty() {
            return format!("{name}: {msg}");
        }
        return name;
    }
    if let Some(s) = ex.as_string().and_then(|s| s.to_string().ok()) {
        return s;
    }
    "<unknown JS exception>".to_string()
}

// Anti-bot challenge detector. Aligned with private-core's
// challenge_detection.py: vendor names, confidence scores, "matched" patterns.
// Adds two fields private-core doesn't carry yet (clearance_cookie hint, status)
// so a router downstream can decide WHICH cookie to extract on escalation.
//
// Returns the *highest-confidence* match, not the first. Returns None on the
// happy path (no signatures matched).
fn detect_challenge(status: u16, body: &str) -> Option<Value> {
    // Cheap early-out: very large 2xx responses are almost certainly real
    // content (article pages, marketplace results). Real challenge pages are
    // typically under 50KB; the 80KB threshold buys headroom while still
    // catching cases like eBay's "Pardon Our Interruption" interstitial
    // (200 + 13KB, would be missed by an 8KB threshold).
    if (200..300).contains(&status) && body.len() > 80_000 {
        return None;
    }
    let lower = body.to_lowercase();

    // (vendor, confidence, &[lowercase substrings to match], clearance_cookie_hint)
    // Patterns are case-insensitive (we lowercase the body once). Substring match,
    // not regex — we don't pull in a regex crate just for this.
    type Group = (&'static str, f64, &'static [&'static str], &'static str);
    let groups: &[Group] = &[
        ("arkose_labs", 0.98, &["arkoselabs", "funcaptcha"], ""),
        (
            "cloudflare_turnstile",
            0.97,
            &[
                "just a moment",
                "checking your browser",
                "cf-challenge",
                "cf_challenge",
                "turnstile",
                "__cf_chl_",
                "cf-mitigated",
            ],
            "cf_clearance",
        ),
        (
            "aws_waf",
            0.96,
            &[
                "awswafcookiedomainlist",
                "gokuprops",
                "aws-waf-token",
                "/awswaf/",
                "challenge.js",
            ],
            "aws-waf-token",
        ),
        (
            "recaptcha",
            0.95,
            &[
                "g-recaptcha",
                "google recaptcha",
                "recaptcha/api2",
                "i'm not a robot",
                "im not a robot",
            ],
            "",
        ),
        (
            "perimeterx_block",
            0.94,
            &[
                "px-captcha",
                "_pxappid",
                "/_px",
                "robot or human",
                "/blocked?url=",
            ],
            "_px3",
        ),
        (
            "datadome",
            0.93,
            &["datadome", "captcha-delivery"],
            "datadome",
        ),
        (
            "press_hold",
            0.92,
            &[
                "press & hold",
                "press and hold",
                "press&hold",
                "hold to confirm",
            ],
            "",
        ),
        (
            "yahoo_sad_panda",
            0.90,
            &[
                "sad-panda",
                "sorry, the page you requested cannot be found",
                "yahoo.*nytransit",
            ],
            "",
        ),
        (
            "akamai_bmp",
            0.88,
            &["_abck=", "bm_sz=", "akamai bot manager"],
            "_abck",
        ),
        (
            "imperva",
            0.85,
            &["_incapsula", "incident_id"],
            "incap_ses_*",
        ),
        // "Pardon Our Interruption" / "Are you a robot" interstitials —
        // typically status 200 + small body + a friendly title. eBay,
        // Distil Networks-class, some Imperva deployments. Confidence set
        // above cloudflare_turnstile (0.97) because these phrases are MORE
        // specific than "checking your browser" / "just a moment" — eBay's
        // Distil page contains both, and we want the specific match to win.
        (
            "interstitial",
            0.99,
            &[
                "pardon our interruption",
                "are you a robot",
                "are you a human",
                "automated access has been blocked",
                "your browser has been flagged",
                "as you were browsing",
            ],
            "",
        ),
        (
            "generic_human_verification",
            0.76,
            &[
                "verify you are human",
                "verify that you are human",
                "unusual traffic",
                "access to this page has been denied",
                "access denied",
                "automated requests",
                "sorry, you have been blocked",
            ],
            "",
        ),
    ];

    let mut best: Option<(&'static str, f64, &'static str, Vec<&'static str>)> = None;
    for (vendor, confidence, patterns, cookie) in groups {
        let matches: Vec<&'static str> = patterns
            .iter()
            .copied()
            .filter(|p| lower.contains(*p))
            .collect();
        if !matches.is_empty() && best.as_ref().is_none_or(|(_, c, _, _)| *confidence > *c) {
            best = Some((*vendor, *confidence, *cookie, matches));
        }
    }

    // Fallback: tiny-body + status anomaly = soft block from an unknown vendor.
    // Conservative thresholds so legitimate small 4xx pages on real sites don't trip it:
    //   - 4xx/5xx OR an unusual 2xx status like 202 (used by AWS WAF)
    //   - body shorter than 5KB (real error pages are usually fuller)
    //   - no specific signature already matched
    if best.is_none() {
        let anomalous_status = !matches!(status, 200 | 301 | 302 | 304 | 404 | 410)
            && (status >= 400 || status == 202 || status == 401 || status == 403);
        if anomalous_status && body.len() < 5000 {
            return Some(json!({
                "blocked": true,
                "provider": "unknown_block",
                "confidence": 0.55,
                "status": status,
                "matched": [],
                "clearance_cookie": Value::Null,
                "reason": format!("Tiny body ({} bytes) on anomalous status {} with no known vendor signature — likely a soft block.", body.len(), status),
                "hint": "Inspect `body` to identify the vendor, escalate to real Chrome to confirm the page renders, or skip this URL.",
            }));
        }
    }

    best.map(|(vendor, confidence, cookie, matches)| {
        json!({
            "blocked": true,
            "provider": vendor,
            "confidence": confidence,
            "status": status,
            "matched": matches,
            "clearance_cookie": if cookie.is_empty() { Value::Null } else { Value::String(cookie.to_string()) },
            "reason": format!("Matched {vendor} challenge signatures."),
            "hint": "Solve once in real Chrome (or via unchainedsky CLI), copy the clearance cookie via DevTools, paste with cookies_set, then retry navigate. Cookie typically lasts 30 min – 24 h.",
        })
    })
}

fn parse_html_to_tree(html: &str) -> Value {
    let dom = html5ever::parse_document(RcDom::default(), Default::default())
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .unwrap_or_else(|_| RcDom::default());
    // The Document node's children include doctype + the <html> element.
    for child in dom.document.children.borrow().iter() {
        if let NodeData::Element { name, .. } = &child.data
            && name.local.as_ref() == "html"
        {
            return node_to_json(child);
        }
    }
    json!({"type": "element", "tag": "html", "attrs": {}, "children": []})
}

fn node_to_json(handle: &Handle) -> Value {
    match &handle.data {
        NodeData::Element { name, attrs, .. } => {
            let mut attr_map = serde_json::Map::new();
            for attr in attrs.borrow().iter() {
                attr_map.insert(
                    attr.name.local.to_string(),
                    Value::String(attr.value.to_string()),
                );
            }
            let children: Vec<Value> = handle
                .children
                .borrow()
                .iter()
                .filter_map(child_to_json)
                .collect();
            json!({
                "type": "element",
                "tag": name.local.as_ref(),
                "attrs": Value::Object(attr_map),
                "children": children,
            })
        }
        _ => Value::Null,
    }
}

fn child_to_json(handle: &Handle) -> Option<Value> {
    match &handle.data {
        NodeData::Text { contents } => {
            let s = contents.borrow().to_string();
            Some(json!({"type": "text", "content": s}))
        }
        NodeData::Element { .. } => Some(node_to_json(handle)),
        // Skip Doctype, Comment, ProcessingInstruction, Document.
        _ => None,
    }
}

fn ok_response(id: Value, result: Value) -> Response {
    Response {
        id,
        result: Some(result),
        error: None,
    }
}

fn err_response(id: Value, code: i32, message: impl Into<String>) -> Response {
    Response {
        id,
        result: None,
        error: Some(RpcError {
            code,
            message: message.into(),
        }),
    }
}

fn write_response(out: &mut impl Write, resp: &Response) -> Result<()> {
    writeln!(out, "{}", serde_json::to_string(resp)?)?;
    out.flush()?;
    Ok(())
}

fn emit_event(name: &str, fields: Value) {
    let payload = json!({ "event": name, "data": fields });
    eprintln!("{}", serde_json::to_string(&payload).unwrap_or_default());
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--mcp") {
        mcp_main().await
    } else {
        rpc_main().await
    }
}

async fn rpc_main() -> Result<()> {
    let mut session = Session::new()?;
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    emit_event("ready", json!({ "version": env!("CARGO_PKG_VERSION") }));

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = err_response(Value::Null, -32700, format!("parse error: {e}"));
                write_response(&mut out, &resp)?;
                continue;
            }
        };

        let id = req.id.clone();
        let resp = match req.method.as_str() {
            "eval" => {
                let code = req
                    .params
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("undefined");
                match session.eval(code) {
                    Ok(v) => ok_response(id, v),
                    Err(e) => err_response(id, -1, e.to_string()),
                }
            }
            "navigate" => match req.params.get("url").and_then(|v| v.as_str()) {
                Some(u) => match session.navigate(u).await {
                    Ok(v) => ok_response(id, v),
                    Err(e) => err_response(id, -2, e.to_string()),
                },
                None => err_response(id, -32602, "missing 'url' param"),
            },
            "body" => match &session.last_body {
                Some(b) => ok_response(id, Value::String(b.clone())),
                None => err_response(id, -3, "no body — call navigate first"),
            },
            "query" => match req.params.get("selector").and_then(|v| v.as_str()) {
                Some(s) => match session.query(s) {
                    Ok(v) => ok_response(id, v),
                    Err(e) => err_response(id, -4, e.to_string()),
                },
                None => err_response(id, -32602, "missing 'selector' param"),
            },
            "text" => {
                let s = req
                    .params
                    .get("selector")
                    .and_then(|v| v.as_str())
                    .unwrap_or("body");
                match session.text(s) {
                    Ok(v) => ok_response(id, v),
                    Err(e) => err_response(id, -5, e.to_string()),
                }
            }
            "text_main" => match session.text_main() {
                Ok(v) => ok_response(id, v),
                Err(e) => err_response(id, -5, e.to_string()),
            },
            "blockmap" => match session.blockmap() {
                Ok(v) => ok_response(id, v),
                Err(e) => err_response(id, -6, e.to_string()),
            },
            "click" => match req.params.get("ref").and_then(|v| v.as_str()) {
                Some(r) => match session.click(r).await {
                    Ok(v) => ok_response(id, v),
                    Err(e) => err_response(id, -7, e.to_string()),
                },
                None => err_response(id, -32602, "missing 'ref' param"),
            },
            "type" => {
                let r = req.params.get("ref").and_then(|v| v.as_str());
                let t = req.params.get("text").and_then(|v| v.as_str());
                match (r, t) {
                    (Some(r), Some(t)) => match session.type_(r, t) {
                        Ok(v) => ok_response(id, v),
                        Err(e) => err_response(id, -8, e.to_string()),
                    },
                    _ => err_response(id, -32602, "missing 'ref' or 'text' param"),
                }
            }
            "submit" => match req.params.get("ref").and_then(|v| v.as_str()) {
                Some(r) => match session.submit(r).await {
                    Ok(v) => ok_response(id, v),
                    Err(e) => err_response(id, -9, e.to_string()),
                },
                None => err_response(id, -32602, "missing 'ref' param"),
            },
            "cookies_set" => {
                let cookies = req.params.get("cookies").and_then(|v| v.as_array());
                let default_url = req
                    .params
                    .get("url")
                    .and_then(|v| v.as_str())
                    .or(session.last_url.as_deref());
                match cookies {
                    Some(arr) => match session.jar.import(arr, default_url) {
                        Ok(n) => ok_response(id, json!({ "added": n })),
                        Err(e) => err_response(id, -10, e.to_string()),
                    },
                    None => err_response(id, -32602, "missing 'cookies' param"),
                }
            }
            "cookies_get" => ok_response(id, Value::Array(session.jar.export())),
            "cookies_clear" => {
                session.jar.clear();
                ok_response(id, json!({ "ok": true }))
            }
            "close" => {
                write_response(&mut out, &ok_response(id, json!("bye")))?;
                return Ok(());
            }
            other => err_response(id, -32601, format!("unknown method: {other}")),
        };
        write_response(&mut out, &resp)?;
    }
    Ok(())
}

// =============================================================================
// MCP server mode (--mcp flag)
//
// Spec: https://modelcontextprotocol.io/  (JSON-RPC 2.0 over stdio)
// Methods we handle: initialize, notifications/initialized, tools/list, tools/call.
// Tool surface = our 12 RPC methods (everything except `close`, which is implicit).
// =============================================================================

fn mcp_tools() -> Value {
    json!([
        {
            "name": "navigate",
            "description": "Fetch a URL with Chrome-fingerprinted HTTP (rquest, Chrome 131 emulation). Parses HTML, seeds the JS DOM, returns a BlockMap (semantic page summary with structure/headings/interactives + ASCII outline) inline. The agent typically does not need a follow-up call to know what's on the page.",
            "inputSchema": {
                "type": "object",
                "properties": { "url": { "type": "string", "description": "Absolute URL to fetch" } },
                "required": ["url"]
            }
        },
        {
            "name": "query",
            "description": "Run a CSS selector against the current page's parsed DOM. Returns matching elements as [{ref, tag, attrs, text}]. Element refs (e:NN) are stable handles for use with click/type/submit. Selector engine supports tag, id, class, attribute matchers (=, ^=, $=, *=, ~=), all four combinators (descendant, >, +, ~), and pseudo-classes (:first/last/nth-child, :first/last/nth-of-type, :only-child/of-type). Does NOT support :not(), :has(), An+B formulas.",
            "inputSchema": {
                "type": "object",
                "properties": { "selector": { "type": "string", "description": "CSS selector" } },
                "required": ["selector"]
            }
        },
        {
            "name": "text",
            "description": "Get the textContent of the FIRST element matching the selector (default: body). Note: on Wikipedia/MDN/news sites, the first <p> is often a hatnote or image caption, not the lead paragraph — prefer `text_main` for reading the page's primary content.",
            "inputSchema": {
                "type": "object",
                "properties": { "selector": { "type": "string", "description": "CSS selector (default: body)" } }
            }
        },
        {
            "name": "text_main",
            "description": "Get the textContent of the page's main content area, excluding chrome (header/nav/footer/aside). Tries <main>, then [role=main], then a single <article>, then falls back to the longest non-chrome subtree. Use this for reading article body / docs page / blog post content.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "blockmap",
            "description": "Recompute the BlockMap for the current page. Use after eval'd JS or click/type modifies the DOM. Same shape as the inline blockmap from navigate.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "click",
            "description": "Dispatch a click event on the element at `ref` (e.g. e:142, returned from query). If the element is <a href> and the click was not preventDefault'd, auto-follows the href via navigate (returns the full navigation result with new BlockMap). Otherwise returns {ok, ref, tag, follow: null}.",
            "inputSchema": {
                "type": "object",
                "properties": { "ref": { "type": "string", "description": "Element ref like e:142" } },
                "required": ["ref"]
            }
        },
        {
            "name": "type",
            "description": "Set the value of an input/textarea (referenced by `ref`) and dispatch input + change events. Use before submit on form fields.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ref": { "type": "string", "description": "Input element ref like e:142" },
                    "text": { "type": "string", "description": "Value to set" }
                },
                "required": ["ref", "text"]
            }
        },
        {
            "name": "submit",
            "description": "Submit a form by gathering its input/textarea/select values, building a query string, and navigating to the resolved action URL. v1 supports GET only; POST/multipart errors out. Skips checkboxes/radios.",
            "inputSchema": {
                "type": "object",
                "properties": { "ref": { "type": "string", "description": "Form element ref like e:142" } },
                "required": ["ref"]
            }
        },
        {
            "name": "body",
            "description": "Return the raw HTML body of the last navigation. Use as a fallback when the BlockMap or selectors aren't enough — but the response can be large (often 100KB+).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "eval",
            "description": "Run arbitrary JavaScript in the embedded QuickJS runtime against the current page's parsed DOM. Returns the JSON-stringified result. Power tool — prefer query/text/blockmap when the CSS selector engine can express what you need.",
            "inputSchema": {
                "type": "object",
                "properties": { "code": { "type": "string", "description": "JS code; the value of the last expression is returned" } },
                "required": ["code"]
            }
        },
        {
            "name": "cookies_set",
            "description": "Add cookies to the session jar. Each item is an object {name, value, domain, path?, secure?, http_only?, url?} or a raw Set-Cookie string. Used to replay clearance cookies (e.g. PerimeterX _px3) lifted from a real Chrome session, bypassing bot detection without running the challenge JS.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cookies": { "type": "array", "description": "Array of cookie objects or Set-Cookie strings" },
                    "url": { "type": "string", "description": "Default URL for cookies that don't specify domain" }
                },
                "required": ["cookies"]
            }
        },
        {
            "name": "cookies_get",
            "description": "Return all cookies currently in the jar as [{name, value, domain, path, secure, http_only}]. Use this to export cookies to disk for a later session.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "cookies_clear",
            "description": "Drop all cookies from the jar.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

async fn dispatch_tool(session: &mut Session, name: &str, args: &Value) -> Result<Value> {
    let str_arg = |k: &str| args.get(k).and_then(|v| v.as_str());
    match name {
        "navigate" => {
            let url = str_arg("url").ok_or_else(|| anyhow!("missing 'url'"))?;
            session.navigate(url).await
        }
        "query" => {
            let sel = str_arg("selector").ok_or_else(|| anyhow!("missing 'selector'"))?;
            session.query(sel)
        }
        "text" => {
            let sel = str_arg("selector").unwrap_or("body");
            session.text(sel)
        }
        "text_main" => session.text_main(),
        "blockmap" => session.blockmap(),
        "click" => {
            let r = str_arg("ref").ok_or_else(|| anyhow!("missing 'ref'"))?;
            session.click(r).await
        }
        "type" => {
            let r = str_arg("ref").ok_or_else(|| anyhow!("missing 'ref'"))?;
            let t = str_arg("text").ok_or_else(|| anyhow!("missing 'text'"))?;
            session.type_(r, t)
        }
        "submit" => {
            let r = str_arg("ref").ok_or_else(|| anyhow!("missing 'ref'"))?;
            session.submit(r).await
        }
        "body" => match &session.last_body {
            Some(b) => Ok(Value::String(b.clone())),
            None => Err(anyhow!("no body — call navigate first")),
        },
        "eval" => {
            let code = str_arg("code").ok_or_else(|| anyhow!("missing 'code'"))?;
            session.eval(code)
        }
        "cookies_set" => {
            let cookies = args
                .get("cookies")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("missing 'cookies'"))?;
            let default_url = str_arg("url").or(session.last_url.as_deref());
            let added = session.jar.import(cookies, default_url)?;
            Ok(json!({ "added": added }))
        }
        "cookies_get" => Ok(Value::Array(session.jar.export())),
        "cookies_clear" => {
            session.jar.clear();
            Ok(json!({ "ok": true }))
        }
        _ => Err(anyhow!("unknown tool: {name}")),
    }
}

async fn mcp_main() -> Result<()> {
    let mut session = Session::new()?;
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") }
                });
                writeln!(out, "{}", serde_json::to_string(&resp)?)?;
                out.flush()?;
                continue;
            }
        };

        let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let id = req.get("id").cloned();
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        let is_notification = id.is_none();

        // Notifications never get a response.
        if method == "notifications/initialized" || method == "notifications/cancelled" {
            continue;
        }

        let result: Result<Value> = match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "unbrowse",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": mcp_tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
                match dispatch_tool(&mut session, name, &arguments).await {
                    Ok(value) => {
                        let text = serde_json::to_string_pretty(&value)?;
                        Ok(json!({
                            "content": [{ "type": "text", "text": text }],
                            "isError": false
                        }))
                    }
                    Err(e) => Ok(json!({
                        "content": [{ "type": "text", "text": format!("Error: {e}") }],
                        "isError": true
                    })),
                }
            }
            _ => Err(anyhow!("method not found: {method}")),
        };

        if is_notification {
            continue;
        }

        let resp = match result {
            Ok(value) => json!({
                "jsonrpc": "2.0",
                "id": id.unwrap_or(Value::Null),
                "result": value
            }),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": id.unwrap_or(Value::Null),
                "error": { "code": -32601, "message": e.to_string() }
            }),
        };
        writeln!(out, "{}", serde_json::to_string(&resp)?)?;
        out.flush()?;
    }
    Ok(())
}
