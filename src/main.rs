use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result, anyhow};
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use rquickjs::FromJs;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};

mod bytecode_cache;
mod network_store;
mod policy;
mod prefit;
mod profile;
use profile::Profile;

const DOM_JS: &str = include_str!("js/dom.js");
const SHIMS_JS: &str = include_str!("js/shims.js");
const BLOCKMAP_JS: &str = include_str!("js/blockmap.js");
const INTERACT_JS: &str = include_str!("js/interact.js");
const EXTRACT_JS: &str = include_str!("js/extract.js");

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

// =============================================================================
// Fetch worker — lets page-script `fetch()` calls go through the same
// rquest::Client we use for navigate (so cookies + Chrome 131 TLS fingerprint
// stay coherent). One dedicated thread, one current_thread tokio runtime,
// requests serialized through an mpsc channel. Responses queue into a shared
// Mutex<Vec<...>> that JS drains via __host_drain_fetches() during settle().
// =============================================================================

struct FetchRequest {
    id: u64,
    method: String,
    url: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Serialize)]
struct FetchResponse {
    id: u64,
    status: u16,
    headers: HashMap<String, String>,
    body: String,
    error: Option<String>,
}

struct FetchQueue {
    sender: mpsc::Sender<FetchRequest>,
    results: Arc<Mutex<Vec<FetchResponse>>>,
    network_store: Arc<Mutex<network_store::NetworkStore>>,
    /// nav_id of the currently-running navigate call, set by navigate_with
    /// at start (after seed_dom) and updated each navigation. The worker
    /// thread reads this when capturing fetches so each capture is bound
    /// to whichever navigation was in flight when it resolved. Prevents
    /// page A's captures from leaking into page B's summary. (PR #7
    /// review medium.)
    current_nav_id: Arc<Mutex<Option<String>>>,
}

fn spawn_fetch_worker(http: rquest::Client) -> FetchQueue {
    let (tx, rx) = mpsc::channel::<FetchRequest>();
    let results: Arc<Mutex<Vec<FetchResponse>>> = Arc::new(Mutex::new(Vec::new()));
    let results_for_thread = results.clone();
    let network_store: Arc<Mutex<network_store::NetworkStore>> =
        Arc::new(Mutex::new(network_store::NetworkStore::default()));
    let store_for_thread = network_store.clone();
    let current_nav_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let nav_id_for_thread = current_nav_id.clone();

    std::thread::Builder::new()
        .name("unbrowser-fetch".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            runtime.block_on(async move {
                while let Ok(req) = rx.recv() {
                    // Snapshot before move — FetchResponse doesn't carry url/method.
                    let url = req.url.clone();
                    let method = req.method.clone();
                    let resp = run_fetch(http.clone(), req).await;
                    // Network capture: opportunistic content-bearing
                    // response capture for the network_stores RPC. See
                    // src/network_store.rs. Skipped for blocked URLs
                    // because the policy hook in __host_fetch_send
                    // short-circuits BEFORE this worker ever sees them
                    // (synthetic 204 enqueued directly to results), so
                    // tracker bodies are never even fetched.
                    //
                    // nav_id binding: read whichever navigate is currently
                    // in flight (or the most recent one that ran) — this
                    // gives each capture a stable navigation_id for
                    // per-page filtering.
                    if resp.error.is_none() && !resp.body.is_empty() {
                        let nav_id = nav_id_for_thread.lock().ok().and_then(|g| g.clone());
                        if let Ok(mut s) = store_for_thread.lock() {
                            s.maybe_capture(
                                &url,
                                &method,
                                resp.status,
                                &resp.headers,
                                &resp.body,
                                nav_id.as_deref(),
                            );
                        }
                    }
                    if let Ok(mut g) = results_for_thread.lock() {
                        g.push(resp);
                    }
                }
            });
        })
        .ok();

    FetchQueue {
        sender: tx,
        results,
        network_store,
        current_nav_id,
    }
}

async fn run_fetch(http: rquest::Client, req: FetchRequest) -> FetchResponse {
    let method = match req.method.to_uppercase().as_str() {
        "GET" => http::Method::GET,
        "POST" => http::Method::POST,
        "PUT" => http::Method::PUT,
        "DELETE" => http::Method::DELETE,
        "HEAD" => http::Method::HEAD,
        "PATCH" => http::Method::PATCH,
        "OPTIONS" => http::Method::OPTIONS,
        _ => http::Method::GET,
    };
    let mut builder = http.request(method, &req.url);
    for (k, v) in &req.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    if !req.body.is_empty() {
        builder = builder.body(req.body.clone());
    }
    match builder.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let mut hmap = HashMap::new();
            for (n, v) in resp.headers() {
                hmap.insert(
                    n.as_str().to_lowercase(),
                    v.to_str().unwrap_or("").to_string(),
                );
            }
            let body = resp.text().await.unwrap_or_default();
            FetchResponse {
                id: req.id,
                status,
                headers: hmap,
                body,
                error: None,
            }
        }
        Err(e) => FetchResponse {
            id: req.id,
            status: 0,
            headers: HashMap::new(),
            body: String::new(),
            error: Some(e.to_string()),
        },
    }
}

struct Session {
    // Holds the QuickJS runtime alive for the Context's lifetime AND
    // exposes execute_pending_job() / is_job_pending() so settle() can
    // drain the microtask queue between timer firings.
    js_rt: rquickjs::Runtime,
    js_ctx: rquickjs::Context,
    http: rquest::Client,
    jar: Arc<CookieJar>,
    // Fetch worker queue — held to keep the worker thread alive and to
    // expose results to settle() via __pollFetches() driven by the JS layer.
    _fetch: Arc<FetchQueue>,
    // Global eval-time deadline (unix-ms). 0 = no deadline. Read by the
    // QuickJS interrupt handler installed once at Session::new and bumped
    // by the per-RPC dispatcher and the navigate script phase. Without
    // this every exec_scripts=true call on a hostile SPA could leave a
    // CPU-pegged process behind.
    eval_deadline_ms: Arc<AtomicU64>,
    last_url: Option<String>,
    last_body: Option<String>,
    // True when --policy=blocklist (or UNBROWSER_POLICY=blocklist) is set.
    // Read by the external-script fetch loop in navigate_with and by the
    // __host_fetch_send hook — see src/policy.rs.
    policy_block: bool,
    // Monotonic counter for navigation_id. Each navigate() call increments
    // and emits a navigation_started event with the new id. Subsequent
    // events from that navigation (script_decision, policy_trace) carry
    // the same id so a driver can join outcomes against decisions.
    // See docs/probabilistic-policy.md §4.5 (outcome protocol).
    //
    // Ordering: Relaxed is correct today (single Session, single QuickJS
    // runtime, current-thread tokio runtime — `navigate_with` cannot run
    // concurrently with itself). If concurrency is ever introduced, the
    // counter would still produce unique ids, but the *visibility* of
    // associated emissions would need at least AcqRel.
    nav_counter: AtomicU64,
    // Set of nav_ids that this Session has issued via next_nav_id() and
    // that have at least reached the navigation_started emit point. Read
    // by report_outcome to reject outcomes for unknown ids. Bounded by
    // number of navigates per process — small in practice; if it ever
    // matters we can switch to a ring buffer.
    nav_ids_issued: Mutex<HashSet<String>>,
    /// Bytecode cache config for eval_with_cache. shim_hash incorporates
    /// the JS environment (shims.js + dom.js) so cached bytecode whose
    /// captured globals diverge from the current build is rejected
    /// automatically. Disabled when UNBROWSER_NO_BYTECODE_CACHE=1.
    bytecode_cache_root: std::path::PathBuf,
    shim_hash: String,
    bytecode_cache_disabled: bool,
    /// Loaded at Session::new from the embedded JSON bundle. Each navigate
    /// looks up the target domain to apply per-(domain, framework)
    /// decision parameters trained centrally — extends the global
    /// Tier-1 blocklist with per-site additions, surfaces settle hints
    /// via the prefit_applied event. None on parse failure (rare; would
    /// be a build-time bug). See src/prefit.rs (R1 from white paper §6).
    prefit: Option<prefit::PrefitBundle>,
}

impl Session {
    fn new(profile: &Profile, policy_block: bool) -> Result<Self> {
        let js_rt = rquickjs::Runtime::new().context("rquickjs Runtime::new")?;
        let js_ctx = rquickjs::Context::full(&js_rt).context("rquickjs Context::full")?;

        // Install the always-on watchdog. Every nested QuickJS eval (including
        // ones inside settle's __pumpTimers callbacks and __pollFetches
        // resolvers) consults this atomic. Default 0 = no bound; the dispatcher
        // bumps it before each RPC call.
        let eval_deadline_ms: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
        let dl_for_handler = eval_deadline_ms.clone();
        js_rt.set_interrupt_handler(Some(Box::new(move || {
            let deadline = dl_for_handler.load(Ordering::Relaxed);
            if deadline == 0 {
                return false;
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            now >= deadline
        })));
        let jar = Arc::new(CookieJar::default());
        let http = rquest::Client::builder()
            .emulation(profile.emulation)
            .cookie_provider(jar.clone())
            // .emulation(...) appears to clobber the default redirect policy.
            // Explicit follow-up-to-10 matches Chrome's behavior on http://github.com,
            // httpbin.org/redirect/N, and the Yahoo "sad panda" 301 chain.
            .redirect(rquest::redirect::Policy::limited(10))
            .build()
            .context("rquest client build")?;
        // Spawn the fetch worker thread (uses the same rquest::Client so cookies
        // + TLS fingerprint stay coherent with navigate).
        let fetch = Arc::new(spawn_fetch_worker(http.clone()));

        // Install JS layers in order:
        //   1. dom.js     — document, Element, querySelector, __seedDOM, etc.
        //   2. shims.js   — passive browser globals (window, navigator, location,
        //                   storage, etc.) — coherent with our Chrome 131 TLS FP
        //   3. blockmap.js — __blockmap() page-summary walker
        //   4. interact.js — __click, __type, __byRef, __formData
        // Then register host bindings the JS layer references at call time
        // (__host_fetch_send, __host_drain_fetches).
        js_ctx.with(|ctx| -> Result<()> {
            ctx.eval::<(), _>(DOM_JS)
                .map_err(|e| anyhow!("eval dom.js: {e}"))?;
            ctx.eval::<(), _>(SHIMS_JS)
                .map_err(|e| anyhow!("eval shims.js: {e}"))?;
            ctx.eval::<(), _>(BLOCKMAP_JS)
                .map_err(|e| anyhow!("eval blockmap.js: {e}"))?;
            ctx.eval::<(), _>(INTERACT_JS)
                .map_err(|e| anyhow!("eval interact.js: {e}"))?;
            ctx.eval::<(), _>(EXTRACT_JS)
                .map_err(|e| anyhow!("eval extract.js: {e}"))?;
            // Apply profile-driven navigator.* patches AFTER shims.js
            // installs the base navigator object. Page scripts that read
            // navigator.userAgent / .platform / .languages now see the
            // profile values, coherent with the TLS+H2 emulation above.
            ctx.eval::<(), _>(profile.js_init())
                .map_err(|e| anyhow!("eval profile.js_init: {e}"))?;

            // __host_fetch_send(id, method, url, headers_json, body) — fire-and-forget.
            // headers_json is a JSON-encoded string from JS to avoid converting
            // an rquickjs Object inside the host closure.
            //
            // Policy hook: when policy_block is on, decide(url) gates the send.
            // Blocked URLs short-circuit with a synthetic 204 pushed straight
            // into the results queue — JS sees the same Promise resolution
            // shape it would for a network-completed request, just with empty
            // body and no actual HTTP made. See src/policy.rs.
            let sender = fetch.sender.clone();
            let results_for_block = fetch.results.clone();
            let host_send = rquickjs::Function::new(
                ctx.clone(),
                move |id: f64, method: String, url: String, headers_json: String, body: String| {
                    if policy_block {
                        let d = policy::decide(&url);
                        if d.blocked {
                            emit_event(
                                "policy_blocked",
                                json!({
                                    "url": url,
                                    "category": d.category.map(|c| c.as_str()),
                                    "matched": d.matched_pattern,
                                    "method": method,
                                }),
                            );
                            if let Ok(mut g) = results_for_block.lock() {
                                g.push(FetchResponse {
                                    id: id as u64,
                                    status: 204,
                                    headers: HashMap::new(),
                                    body: String::new(),
                                    error: None,
                                });
                            }
                            return;
                        }
                    }
                    let mut hmap: HashMap<String, String> = HashMap::new();
                    if let Ok(serde_json::Value::Object(map)) =
                        serde_json::from_str::<serde_json::Value>(&headers_json)
                    {
                        for (k, v) in map {
                            if let Some(s) = v.as_str() {
                                hmap.insert(k, s.to_string());
                            }
                        }
                    }
                    let req = FetchRequest {
                        id: id as u64,
                        method,
                        url,
                        headers: hmap,
                        body: body.into_bytes(),
                    };
                    let _ = sender.send(req);
                },
            )
            .map_err(|e| anyhow!("install __host_fetch_send: {e}"))?;
            ctx.globals()
                .set("__host_fetch_send", host_send)
                .map_err(|e| anyhow!("set __host_fetch_send: {e}"))?;

            // __host_drain_fetches() -> JSON-encoded array of pending FetchResponse.
            // JS-side parses and resolves the corresponding Promises.
            let results = fetch.results.clone();
            let host_drain = rquickjs::Function::new(ctx.clone(), move || -> String {
                let mut guard = match results.lock() {
                    Ok(g) => g,
                    Err(_) => return "[]".to_string(),
                };
                let drained: Vec<FetchResponse> = guard.drain(..).collect();
                drop(guard);
                serde_json::to_string(&drained).unwrap_or_else(|_| "[]".to_string())
            })
            .map_err(|e| anyhow!("install __host_drain_fetches: {e}"))?;
            ctx.globals()
                .set("__host_drain_fetches", host_drain)
                .map_err(|e| anyhow!("set __host_drain_fetches: {e}"))?;

            // __host_resolve_url(src, base) — delegates to Rust's url::Url::join,
            // which is fully spec-compliant (handles ../, ./, query-only,
            // fragment-only, scheme-relative). Used by dom.js's dynamic-script
            // loader so dynamic chunks resolve correctly. (PR #6 review medium.)
            // Returns the input src on parse failure — caller decides whether
            // to fall back to the JS-side regex resolver.
            let host_resolve_url =
                rquickjs::Function::new(ctx.clone(), |src: String, base: String| -> String {
                    if src.is_empty() {
                        return src;
                    }
                    match url::Url::parse(&base) {
                        Ok(b) => b.join(&src).map(|u| u.to_string()).unwrap_or(src),
                        Err(_) => src,
                    }
                })
                .map_err(|e| anyhow!("install __host_resolve_url: {e}"))?;
            ctx.globals()
                .set("__host_resolve_url", host_resolve_url)
                .map_err(|e| anyhow!("set __host_resolve_url: {e}"))?;

            // __host_parse_html_fragment(html) — parses an HTML fragment
            // string into the same JSON tree shape main.rs's full document
            // parser produces. Used by dom.js's Element.innerHTML setter
            // and insertAdjacentHTML(); without it those silently no-op.
            // Context element is <body> (matches what real browsers do
            // for innerHTML on most elements). Returns a fragment-rooted
            // tree as JSON; caller JSON.parses and feeds to buildChildren.
            // (Implements piece #2 from the SPA-content-extraction proposal.)
            let host_parse_fragment =
                rquickjs::Function::new(ctx.clone(), |html: String| -> String {
                    parse_html_fragment_to_json(&html)
                })
                .map_err(|e| anyhow!("install __host_parse_html_fragment: {e}"))?;
            ctx.globals()
                .set("__host_parse_html_fragment", host_parse_fragment)
                .map_err(|e| anyhow!("set __host_parse_html_fragment: {e}"))?;

            Ok(())
        })?;
        // Bytecode cache setup: hash the JS env so cache files invalidate
        // automatically on shims.js / dom.js changes. Prune once at startup
        // so the cap is honored across many process lifetimes — we don't
        // pay the directory walk on every cache hit.
        let bytecode_cache_disabled = bytecode_cache::is_disabled();
        let bytecode_cache_root = bytecode_cache::cache_dir();
        let shim_hash = bytecode_cache::sha256(&format!(
            "{DOM_JS}\0{SHIMS_JS}\0{BLOCKMAP_JS}\0{INTERACT_JS}"
        ));
        if !bytecode_cache_disabled {
            bytecode_cache::prune(&bytecode_cache_root, bytecode_cache::max_total_bytes());
        }
        let prefit = prefit::PrefitBundle::load_embedded();
        Ok(Self {
            js_rt,
            js_ctx,
            http,
            jar,
            _fetch: fetch,
            eval_deadline_ms,
            last_url: None,
            last_body: None,
            policy_block,
            nav_counter: AtomicU64::new(0),
            nav_ids_issued: Mutex::new(HashSet::new()),
            bytecode_cache_root,
            shim_hash,
            bytecode_cache_disabled,
            prefit,
        })
    }

    // Generate the next navigation_id for events emitted by navigate_with.
    // Format `nav_<n>` keeps it grep-friendly and short. Within a single
    // session (process lifetime) ids are unique and monotonic; not globally
    // unique — drivers that need cross-session correlation should pair this
    // with their own session id.
    fn next_nav_id(&self) -> String {
        let n = self.nav_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("nav_{n}");
        if let Ok(mut set) = self.nav_ids_issued.lock() {
            set.insert(id.clone());
        }
        id
    }

    fn nav_id_is_known(&self, id: &str) -> bool {
        self.nav_ids_issued
            .lock()
            .map(|set| set.contains(id))
            .unwrap_or(false)
    }

    // Set a wall-clock deadline (ms from now) that bounds every JS eval until
    // restored. Returns the previous deadline so the caller can restore it
    // (supports nested deadlines — the script phase tightens the navigate
    // budget, then restores the outer dispatcher budget). A budget of 0 means
    // "leave it unbounded"; the dispatcher should never call that.
    fn set_eval_deadline_from_now(&self, ms: u64) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let new_dl = now.saturating_add(ms);
        self.eval_deadline_ms.swap(new_dl, Ordering::Relaxed)
    }

    fn restore_eval_deadline(&self, prev: u64) {
        self.eval_deadline_ms.store(prev, Ordering::Relaxed);
    }

    // Eval that doesn't try to JSON.stringify the result. Right tool for
    // executing page <script> tags whose last expression often returns a
    // DOM Element (circular refs → stringify throws). Surfaces real JS
    // errors via ctx.catch() like eval() does.
    fn eval_void(&self, code: &str) -> Result<()> {
        self.js_ctx.with(|ctx| -> Result<()> {
            match ctx.eval::<rquickjs::Value, _>(code) {
                Ok(_) => Ok(()),
                Err(rquickjs::Error::Exception) => {
                    Err(anyhow!("{}", format_js_exception(ctx.catch())))
                }
                Err(e) => Err(anyhow!("js eval: {e}")),
            }
        })
    }

    // Eval with bytecode cache. On hit, skips the QuickJS parse phase
    // (the dominant cost on heavy React/Next bundles). On miss, compiles,
    // writes bytecode to disk, then executes. `name` is a debug-friendly
    // identifier (URL or "inline-{hash}") used for events and source-map-
    // style stack frames. Falls back to plain eval on any cache failure
    // so caching is opportunistic — never blocks correctness.
    //
    // See src/bytecode_cache.rs for the unsafe QuickJS glue.
    fn eval_with_cache(&self, source: &str, name: &str) -> Result<()> {
        if self.bytecode_cache_disabled {
            return self.eval_void(source);
        }
        let key = bytecode_cache::cache_key(source, &self.shim_hash);
        let root = &self.bytecode_cache_root;
        // The eval-deadline watchdog must be SUSPENDED only across
        // compile_to_bytecode (JS_Eval COMPILE_ONLY) — the watchdog
        // spuriously aborts on >~16KB scripts during the parse-only path.
        // It MUST stay armed across load_and_eval, which actually runs
        // user code and can loop forever. Earlier code suspended it for
        // the whole closure, which let cached bundles run unbounded.
        let dl = self.eval_deadline_ms.clone();
        let result = self.js_ctx.with(|ctx| -> Result<()> {
            // Try cache. Watchdog stays armed — load_and_eval runs user code.
            if let Some(bytes) = bytecode_cache::read(root, &key) {
                let bytes_len = bytes.len();
                match bytecode_cache::load_and_eval(&ctx, &bytes) {
                    Ok(()) => {
                        emit_event(
                            "bytecode_cache",
                            json!({
                                "schema_version": 1,
                                "hit": true,
                                "name": name,
                                "bytes": bytes_len,
                            }),
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        // Could be a true JS exception OR a watchdog interrupt
                        // (cached bundle ran past the deadline). Surface either
                        // back to the caller so script_executed gets the right
                        // error and `interrupted` flag.
                        emit_event(
                            "bytecode_cache",
                            json!({
                                "schema_version": 1,
                                "hit": true,
                                "name": name,
                                "load_error": e,
                            }),
                        );
                        let caught = ctx.catch();
                        return if caught.is_null() || caught.is_undefined() {
                            Err(anyhow!("bytecode eval threw (no exception captured)"))
                        } else {
                            Err(anyhow!("{}", format_js_exception(caught)))
                        };
                    }
                }
            }
            // Cache miss: compile (watchdog suspended just for this call),
            // persist, then execute via the freshly-produced bytecode.
            let prev_dl = dl.swap(0, Ordering::Relaxed);
            let compile_res = bytecode_cache::compile_to_bytecode(&ctx, source, name);
            dl.store(prev_dl, Ordering::Relaxed);
            match compile_res {
                Ok(bytes) => {
                    let _ = bytecode_cache::write(root, &key, &bytes);
                    let bytes_len = bytes.len();
                    let result = bytecode_cache::load_and_eval(&ctx, &bytes);
                    emit_event(
                        "bytecode_cache",
                        json!({
                            "schema_version": 1,
                            "hit": false,
                            "name": name,
                            "compiled_bytes": bytes_len,
                        }),
                    );
                    match result {
                        Ok(()) => Ok(()),
                        // Eval-time exception OR watchdog interrupt.
                        Err(_) => {
                            let caught = ctx.catch();
                            if caught.is_null() || caught.is_undefined() {
                                Err(anyhow!("bytecode eval threw (no exception captured)"))
                            } else {
                                Err(anyhow!("{}", format_js_exception(caught)))
                            }
                        }
                    }
                }
                Err(e) => {
                    // Compile failure (SyntaxError, OOM, or QuickJS refusal).
                    // Surface via NDJSON so drivers see why caching skipped.
                    // Fall back to plain eval — matches eval_void's
                    // exception path so JS-level errors still surface.
                    emit_event(
                        "bytecode_cache",
                        json!({
                            "schema_version": 1,
                            "hit": false,
                            "name": name,
                            "compile_error": e,
                        }),
                    );
                    match ctx.eval::<rquickjs::Value, _>(source) {
                        Ok(_) => Ok(()),
                        Err(rquickjs::Error::Exception) => {
                            Err(anyhow!("{}", format_js_exception(ctx.catch())))
                        }
                        Err(e) => Err(anyhow!("js eval: {e}")),
                    }
                }
            }
        });
        result
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

    async fn navigate(&mut self, url: &str, exec_scripts: bool) -> Result<Value> {
        self.navigate_with(self.http.get(url), exec_scripts).await
    }

    // Shared pipeline: take an already-built rquest::RequestBuilder (GET from
    // navigate, POST from submit), send it, run it through DOM seeding,
    // BlockMap, challenge detection, and optional script execution. Keeps
    // GET and POST coherent on cookies/TLS-FP/redirect handling without a
    // second copy of the post-fetch logic.
    async fn navigate_with(
        &mut self,
        req: rquest::RequestBuilder,
        exec_scripts: bool,
    ) -> Result<Value> {
        let nav_start = std::time::Instant::now();
        let nav_id = self.next_nav_id();
        let resp = req.send().await.context("http send")?;
        let status = resp.status().as_u16();
        let final_url = resp.url().to_string();
        // Defer navigation_started until DOM is seeded — pairing invariant:
        // if navigation_started fires, policy_trace WILL fire before this
        // function returns. Errors above this point (http send, body read,
        // DOM seed) propagate without firing either event, so a driver
        // never sees an orphan navigation_id. See review of PR #4 H2.

        // Snapshot useful response headers before consuming the response body.
        // Multi-value headers (Set-Cookie) are joined with ' || ' since they're
        // mostly diagnostic — the actual cookie storage already happened in
        // rquest's CookieStore impl.
        let mut headers: serde_json::Map<String, Value> = serde_json::Map::new();
        // Parallel HashMap for network_store::maybe_capture (it needs a
        // HashMap<String, String>, not the serde_json::Map shape we return).
        let mut headers_flat: HashMap<String, String> = HashMap::new();
        for (name, value) in resp.headers() {
            let key = name.as_str().to_lowercase();
            let v = value.to_str().unwrap_or("").to_string();
            match headers.get_mut(&key) {
                Some(Value::String(existing)) => {
                    *existing = format!("{existing} || {v}");
                }
                _ => {
                    headers.insert(key.clone(), Value::String(v.clone()));
                }
            }
            // For the network store: keep one value per name (last wins);
            // the only multi-value header that matters here is Set-Cookie
            // and it's not used for content-type classification.
            headers_flat.insert(key, v);
        }

        let body = resp.text().await.context("read body")?;
        let bytes = body.len();

        // Capture the navigate response itself into the network store.
        // JSON-shaped landing pages (raw GraphQL endpoints, JSON feeds,
        // route-data preloads) get surfaced via the network_stores RPC
        // alongside fetch/XHR responses from page scripts. The navigate
        // body is often the single most important content-bearing fetch
        // on a JSON-API-shaped site. HTML pages are skipped by the
        // classifier (text/html → score 0).
        if (200..400).contains(&status)
            && !body.is_empty()
            && let Ok(mut s) = self._fetch.network_store.lock()
        {
            s.maybe_capture(
                &final_url,
                "GET",
                status,
                &headers_flat,
                &body,
                Some(&nav_id),
            );
        }

        let challenge = detect_challenge(status, &body);
        if let Some(c) = &challenge {
            emit_event("challenge", c.clone());
        }

        let tree = parse_html_to_tree(&body);
        self.seed_dom(&tree)?;

        // Publish current_nav_id to the worker so any fetch that resolves
        // during this navigate's settle loop is bound to this navigation.
        // Set after seed_dom to match the navigation_started invariant —
        // this nav_id is now "live" until the next navigate overwrites it.
        if let Ok(mut g) = self._fetch.current_nav_id.lock() {
            *g = Some(nav_id.clone());
        }

        // DOM is now committed for this nav_id. From here on, the function
        // path always reaches the policy_trace emission (script branches
        // both emit it; non-exec branch emits a minimal trace). Safe to
        // announce navigation_started.
        emit_event(
            "navigation_started",
            json!({
                "schema_version": 1,
                "navigation_id": nav_id,
                "url": final_url,
                "status": status,
                "bytes": bytes,
                "exec_scripts": exec_scripts,
                "policy_block": self.policy_block,
            }),
        );

        // Prefit lookup (R2 from white paper §6 Track 1). Look up the
        // target host in the embedded prefit bundle. If we have an entry,
        // emit `prefit_applied` so drivers see what per-domain knowledge
        // is in play, and capture the additions for the script-fetch loop
        // below to extend Tier-1 blocking with.
        let prefit_for_domain: Option<&prefit::DomainPrefit> = self.prefit.as_ref().and_then(|b| {
            url::Url::parse(&final_url)
                .ok()
                .and_then(|u| u.host_str().map(|s| s.to_string()))
                .and_then(|host| b.lookup_domain(&host))
        });
        if let Some(p) = prefit_for_domain {
            emit_event(
                "prefit_applied",
                json!({
                    "schema_version": 1,
                    "navigation_id": nav_id,
                    "domain": p.domain,
                    "framework": p.framework,
                    "blocklist_additions": p.blocklist_additions.len(),
                    "shape_hint": p.shape_hint,
                    "settle_distribution": p.settle_distribution,
                }),
            );
        }

        // Update window.location for any page scripts that read it.
        let url_lit = serde_json::to_string(&final_url)?;
        let _ = self.eval(&format!("__setLocation({url_lit})"));

        // Phase 5: optionally execute page scripts (inline + external src).
        let scripts = if exec_scripts && (200..400).contains(&status) {
            let items = collect_scripts(&tree, &final_url);
            let mut inline_count = 0usize;
            let mut external_count = 0usize;
            let mut async_count = 0usize;
            let mut policy_blocked_count = 0usize;
            let mut fetch_errors: Vec<String> = Vec::new();

            // Spawn external fetches in parallel — current_thread runtime
            // interleaves them at network-I/O await points, so a page with
            // N external bundles takes ~max(round-trip times) instead of
            // sum(round-trip times). Each task has a per-fetch timeout so
            // a single huge bundle can't hang the navigate indefinitely.
            // Document ordering preserved by indexing results.
            //
            // Policy hook: when self.policy_block is on, decide(url) gates
            // each external fetch BEFORE we spawn the task. Static <script
            // src> tracker URLs (Adobe DTM, Ketch, GoogleTagServices, etc.)
            // are caught here — they bypass __host_fetch_send because page
            // scripts haven't run yet to issue them, so this is the
            // structurally correct place for the gate. See src/policy.rs.
            const SCRIPT_FETCH_TIMEOUT_MS: u64 = 8000;
            let mut fetch_tasks: Vec<(usize, tokio::task::JoinHandle<Result<String, String>>)> =
                Vec::new();
            // Authoritative record of which script ids were skipped at first
            // pass. Replaces the previous "re-call policy::decide in assembly
            // pass" approach (review M4) — fragile if policy_block toggles
            // mid-navigate or if any non-deterministic structural prior
            // enters policy::decide later. HashSet keeps the assembly pass
            // O(1) per item.
            let mut skipped_ids: HashSet<usize> = HashSet::new();
            for (idx, item) in items.iter().enumerate() {
                if let ScriptItem::External { url: u, kind } = item {
                    let host = host_of(u);
                    let kind_str = script_kind_str(*kind);
                    if self.policy_block {
                        let d = policy::decide(u);
                        if d.blocked {
                            // Spec §6 schema: action enum is small (skip|run|
                            // fetch_failed), reasons compose orthogonally.
                            // Was previously action: "skip_blocklist".
                            emit_event(
                                "script_decision",
                                json!({
                                    "schema_version": 1,
                                    "navigation_id": nav_id,
                                    "script_id": idx,
                                    "url": u,
                                    "host": host,
                                    "kind": kind_str,
                                    "action": "skip",
                                    "reason": "blocklist",
                                    "category": d.category.map(|c| c.as_str()),
                                    "matched": d.matched_pattern,
                                }),
                            );
                            // Legacy event — kept for one cycle for back-compat
                            // with policy_baseline.py / policy_e2e.py. Drop in
                            // a follow-up PR once consumers have switched.
                            emit_event(
                                "policy_blocked",
                                json!({
                                    "url": u,
                                    "kind": "static_script",
                                    "category": d.category.map(|c| c.as_str()),
                                    "matched": d.matched_pattern,
                                }),
                            );
                            policy_blocked_count += 1;
                            skipped_ids.insert(idx);
                            continue;
                        }
                        // Tier-1.5: per-domain blocklist additions from the
                        // prefit bundle. URLs that aren't in the global
                        // Tier-1 list but ARE in this domain's known-tracker
                        // set are also skipped. Reason recorded as
                        // "prefit_blocklist" so drivers can distinguish.
                        if let (Some(bundle), Some(p)) = (self.prefit.as_ref(), prefit_for_domain)
                            && bundle.matches_blocklist_addition(p, u)
                        {
                            emit_event(
                                "script_decision",
                                json!({
                                    "schema_version": 1,
                                    "navigation_id": nav_id,
                                    "script_id": idx,
                                    "url": u,
                                    "host": host,
                                    "kind": kind_str,
                                    "action": "skip",
                                    "reason": "prefit_blocklist",
                                    "domain": p.domain,
                                }),
                            );
                            policy_blocked_count += 1;
                            skipped_ids.insert(idx);
                            continue;
                        }
                    }
                    let url = u.clone();
                    let http = self.http.clone();
                    fetch_tasks.push((
                        idx,
                        tokio::spawn(async move {
                            let fut = async {
                                match http.get(&url).send().await {
                                    Ok(resp) if resp.status().is_success() => {
                                        match resp.text().await {
                                            Ok(body) => Ok(body),
                                            Err(e) => Err(format!("read {url}: {e}")),
                                        }
                                    }
                                    Ok(resp) => {
                                        Err(format!("status {} fetching {}", resp.status(), url))
                                    }
                                    Err(e) => Err(format!("fetch {url}: {e}")),
                                }
                            };
                            match tokio::time::timeout(
                                std::time::Duration::from_millis(SCRIPT_FETCH_TIMEOUT_MS),
                                fut,
                            )
                            .await
                            {
                                Ok(r) => r,
                                Err(_) => Err(format!(
                                    "timeout {SCRIPT_FETCH_TIMEOUT_MS}ms fetching {url}"
                                )),
                            }
                        }),
                    ));
                }
            }
            let mut external_results: HashMap<usize, String> = HashMap::new();
            for (idx, task) in fetch_tasks {
                match task.await {
                    Ok(Ok(body)) => {
                        external_results.insert(idx, body);
                        external_count += 1;
                    }
                    Ok(Err(e)) => fetch_errors.push(e),
                    Err(join_e) => fetch_errors.push(format!("task panicked: {join_e}")),
                }
            }

            // Two-pass assembly to honor `async` script semantics:
            //   sync_sources  — Inline + External(Sync) in document order
            //   async_sources — External(Async) in document order, executed
            //                   AFTER the sync queue. The HTML spec lets async
            //                   scripts execute as soon as their fetch
            //                   completes (no order guarantee w.r.t. other
            //                   scripts); we approximate by executing them
            //                   last in document order, which is spec-legal
            //                   (well-behaved async scripts can't depend on
            //                   ordering anyway) and trivially deterministic
            //                   for replay/measurement. Defer is folded into
            //                   Sync — we have no incremental parsing, so
            //                   "execute after parse in document order"
            //                   collapses to "execute now in document order."
            // Each entry pairs (script_id, kind_str, optional url, body) so
            // the eval loop below can emit a script_executed event per
            // source with the correct script_id and url for credit
            // assignment by future Bayesian phases.
            let mut sync_sources: Vec<(usize, &'static str, Option<String>, String)> = Vec::new();
            let mut async_sources: Vec<(usize, &'static str, Option<String>, String)> = Vec::new();
            let mut fetch_failed_count = 0usize;
            for (idx, item) in items.into_iter().enumerate() {
                match item {
                    ScriptItem::Inline(s) => {
                        inline_count += 1;
                        // No script_decision for inline (v0 emits decisions
                        // for external only — inline scripts always run).
                        sync_sources.push((idx, "inline", None, s));
                    }
                    ScriptItem::External { url, kind } => {
                        if skipped_ids.contains(&idx) {
                            // Already emitted script_decision(skip) at first pass.
                            continue;
                        }
                        let host = host_of(&url);
                        let kind_str = script_kind_str(kind);
                        if let Some(body) = external_results.remove(&idx) {
                            // Spec §6: action enum is run|skip|fetch_failed.
                            // We use "queued" here because eval has not yet
                            // happened — the actual execution outcome is
                            // reported separately via script_executed below.
                            // Drivers wanting "ran successfully" should join
                            // script_decision{action: queued} with
                            // script_executed{error: null}.
                            emit_event(
                                "script_decision",
                                json!({
                                    "schema_version": 1,
                                    "navigation_id": nav_id,
                                    "script_id": idx,
                                    "url": url,
                                    "host": host,
                                    "kind": kind_str,
                                    "action": "queued",
                                }),
                            );
                            match kind {
                                ScriptKind::Sync => {
                                    sync_sources.push((idx, kind_str, Some(url), body));
                                }
                                ScriptKind::Async => {
                                    async_count += 1;
                                    async_sources.push((idx, kind_str, Some(url), body));
                                }
                            }
                        } else {
                            fetch_failed_count += 1;
                            emit_event(
                                "script_decision",
                                json!({
                                    "schema_version": 1,
                                    "navigation_id": nav_id,
                                    "script_id": idx,
                                    "url": url,
                                    "host": host,
                                    "kind": kind_str,
                                    "action": "fetch_failed",
                                }),
                            );
                        }
                    }
                }
            }
            let sources: Vec<(usize, &'static str, Option<String>, String)> =
                sync_sources.into_iter().chain(async_sources).collect();
            // Eval all in document order. Page scripts often end with an
            // Element-returning expression (circular refs → JSON.stringify
            // throws), so use eval_void.
            //
            // Bound total eval time. Heavy React/Vue bundles can run pathological
            // top-level code in QuickJS for tens of seconds; we don't want a
            // single navigate hanging the binary. The watchdog interrupt
            // handler installed in Session::new fires periodically inside
            // QuickJS and aborts any running script (or settle pump callback,
            // or microtask) once the deadline passes. Tighten the outer
            // dispatcher budget to 5s for the script-eval phase, then restore.
            const SCRIPT_EVAL_BUDGET_MS: u64 = 5000;
            let prev_deadline = self.set_eval_deadline_from_now(SCRIPT_EVAL_BUDGET_MS);

            let mut eval_errors: Vec<String> = Vec::new();
            let mut executed: usize = 0;
            let mut interrupted: usize = 0;
            for (script_id, kind_str, url, source) in &sources {
                let eval_start = std::time::Instant::now();
                // Three-way routing:
                //   1. Module-shaped sources (PR #11) → __loadModule, which
                //      recursively loads deps then evals the cleaned body.
                //      Returns a Promise; settle drives it to completion.
                //      Bytecode caching skipped for modules — the loader
                //      strips imports before eval, so the cached bytecode
                //      would not match the public source's hash.
                //   2. Classic sources → eval_with_cache. Hit skips parse;
                //      miss compiles + caches + evals.
                let result = if looks_like_module(source) {
                    let src_lit = serde_json::to_string(source).unwrap_or_default();
                    let url_lit =
                        serde_json::to_string(url.as_deref().unwrap_or("")).unwrap_or_default();
                    self.eval_void(&format!("__loadModule({src_lit}, {url_lit})"))
                } else {
                    let cache_name = url.as_deref().unwrap_or("inline").to_string();
                    self.eval_with_cache(source, &cache_name)
                };
                let duration_us = eval_start.elapsed().as_micros() as u64;
                match result {
                    Err(e) => {
                        let msg = e.to_string();
                        let is_interrupt = msg.contains("interrupted");
                        if is_interrupt {
                            interrupted += 1;
                        }
                        let truncated = if msg.len() > 200 {
                            format!("{}…", &msg[..200])
                        } else {
                            msg.clone()
                        };
                        eval_errors.push(truncated.clone());
                        // Spec §6: script_executed reports actual runtime
                        // outcome, distinct from script_decision (queued).
                        emit_event(
                            "script_executed",
                            json!({
                                "schema_version": 1,
                                "navigation_id": nav_id,
                                "script_id": script_id,
                                "url": url,
                                "kind": kind_str,
                                "duration_us": duration_us,
                                "error": truncated,
                                "interrupted": is_interrupt,
                            }),
                        );
                    }
                    Ok(()) => {
                        executed += 1;
                        emit_event(
                            "script_executed",
                            json!({
                                "schema_version": 1,
                                "navigation_id": nav_id,
                                "script_id": script_id,
                                "url": url,
                                "kind": kind_str,
                                "duration_us": duration_us,
                                "error": Value::Null,
                                "interrupted": false,
                            }),
                        );
                    }
                }
            }

            // Restore the dispatcher's outer deadline so settle's pumps run
            // under the broader navigate budget rather than the tight 5s
            // script-phase one. (Settle pump callbacks are bounded too — they
            // run inside QuickJS evals which still consult the same atomic.)
            self.restore_eval_deadline(prev_deadline);
            // Fire DOMContentLoaded → settle → load → settle. Each settle
            // emits a `settle_exit` event with reason + counts so traces show
            // exactly why we bailed (idle / budget_exhausted / max_iters).
            // Without this, a hung Next.js hydration looks indistinguishable
            // from a clean exit in the NDJSON stream — only `policy_trace`
            // carries the settle blob and it's often truncated.
            let _ = self
                .eval("typeof __fireDOMContentLoaded === 'function' && __fireDOMContentLoaded()");
            let after_dcl = self.settle(2000, 100).await.ok();
            if let Some(r) = &after_dcl {
                emit_event(
                    "settle_exit",
                    json!({
                        "schema_version": 1,
                        "navigation_id": nav_id,
                        "phase": "after_dcl",
                        "result": r,
                    }),
                );
            }
            let _ = self.eval("typeof __fireLoad === 'function' && __fireLoad()");
            let after_load = self.settle(1500, 50).await.ok();
            if let Some(r) = &after_load {
                emit_event(
                    "settle_exit",
                    json!({
                        "schema_version": 1,
                        "navigation_id": nav_id,
                        "phase": "after_load",
                        "result": r,
                    }),
                );
            }
            // Phase A: per-navigation policy trace. One event summarizing
            // every decision made during this navigate, joined to outcomes
            // via navigation_id when the driver later calls report_outcome.
            // See docs/probabilistic-policy.md §4.5.
            emit_event(
                "policy_trace",
                json!({
                    "schema_version": 1,
                    "navigation_id": nav_id,
                    "url": final_url,
                    "policy_block": self.policy_block,
                    "scripts": {
                        "inline": inline_count,
                        "external": external_count,
                        "async": async_count,
                        "skipped_blocklist": policy_blocked_count,
                        "fetch_failed": fetch_failed_count,
                        "executed": executed,
                        "interrupted": interrupted,
                    },
                    "settle": {
                        "after_dcl": after_dcl,
                        "after_load": after_load,
                    },
                    "elapsed_ms": nav_start.elapsed().as_millis() as u64,
                }),
            );
            Some(json!({
                "inline_count": inline_count,
                "external_count": external_count,
                "async_count": async_count,
                "policy_blocked": policy_blocked_count,
                "fetch_failed": fetch_failed_count,
                "executed": executed,
                "interrupted": interrupted,
                "errors_count": eval_errors.len(),
                "errors": eval_errors.into_iter().take(10).collect::<Vec<_>>(),
                "fetch_errors_count": fetch_errors.len(),
                "fetch_errors": fetch_errors.into_iter().take(10).collect::<Vec<_>>(),
                "settle_after_dcl": after_dcl,
                "settle_after_load": after_load,
            }))
        } else {
            // exec_scripts=false: still emit a minimal policy_trace so the
            // driver always has a paired event for navigation_started.
            emit_event(
                "policy_trace",
                json!({
                    "schema_version": 1,
                    "navigation_id": nav_id,
                    "url": final_url,
                    "policy_block": self.policy_block,
                    "scripts": null,
                    "settle": null,
                    "elapsed_ms": nav_start.elapsed().as_millis() as u64,
                }),
            );
            None
        };

        self.last_url = Some(final_url.clone());
        self.last_body = Some(body);

        let blockmap = self.blockmap().unwrap_or(Value::Null);

        // Auto-extract whenever the page embeds JSON-bearing <script> tags
        // (density.json_scripts > 0). Across a 32-site sweep this delivers
        // substantial structured data (JSON-LD article schemas, __NEXT_DATA__
        // page state, json_in_script product blobs, GitHub RSC payloads) on
        // ~15/20 sites where the agent would otherwise have to issue a second
        // extract() call. The earlier conjunctive gate on `likely_js_filled`
        // was empirically inert: shell-shaped pages and JSON-bearing pages are
        // anti-correlated in the wild — if a site is a thin shell it usually
        // fetches data later via XHR; if it embeds JSON it usually rendered
        // enough HTML to not look like a shell.
        //
        // Cost: __extract() is a sync QuickJS eval over the already-parsed
        // DOM (no network, no re-parse). On pages with only meta tags this is
        // sub-ms. On JSON-heavy pages a JSON.parse pass + the FFI roundtrip
        // back through serde_json runs ~20–150ms — bounded by the inline-size
        // cap below so a runaway result can't bloat the navigate response.
        //
        // Inline cap rationale: navigate's response is one JSON-RPC line on
        // stdout. Multi-MB lines choke MCP hosts and naïve readline
        // consumers. 256 KB comfortably fits a large __NEXT_DATA__ (Zillow
        // ~160 KB) but caps pathological Magento PLPs (sometimes 500 KB+ of
        // init blobs). On overflow we return a stub carrying strategy /
        // confidence / size so the agent knows what's there and can call
        // extract() explicitly to retrieve the full payload.
        const MAX_INLINE_EXTRACT_BYTES: usize = 256 * 1024;

        let json_scripts = blockmap
            .get("density")
            .and_then(|d| d.get("json_scripts"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let (auto_extract, auto_extract_error) = if json_scripts > 0 {
            match self.extract(None) {
                Ok(v) => {
                    let size = serde_json::to_string(&v).map(|s| s.len()).unwrap_or(0);
                    if size > MAX_INLINE_EXTRACT_BYTES {
                        let strategy = v.get("strategy").cloned().unwrap_or(Value::Null);
                        let confidence = v.get("confidence").cloned().unwrap_or(Value::Null);
                        (
                            Some(json!({
                                "strategy": strategy,
                                "confidence": confidence,
                                "data": null,
                                "truncated": true,
                                "size_bytes": size,
                                "hint": format!(
                                    "extract result {size} bytes exceeds {MAX_INLINE_EXTRACT_BYTES} byte inline cap; call extract() to retrieve full data"
                                ),
                            })),
                            None,
                        )
                    } else {
                        (Some(v), None)
                    }
                }
                Err(e) => (None, Some(e.to_string())),
            }
        } else {
            (None, None)
        };

        emit_event(
            "navigate",
            json!({
                "url": final_url,
                "status": status,
                "bytes": bytes,
                "elapsed_ms": nav_start.elapsed().as_millis() as u64,
                "exec_scripts": exec_scripts,
                "scripts_executed": scripts.as_ref().and_then(|s| s.get("executed")),
                "scripts_interrupted": scripts.as_ref().and_then(|s| s.get("interrupted")),
                "auto_extract_strategy": auto_extract.as_ref().and_then(|e| e.get("strategy")),
                "auto_extract_confidence": auto_extract.as_ref().and_then(|e| e.get("confidence")),
                "auto_extract_truncated": auto_extract.as_ref().and_then(|e| e.get("truncated")),
                "auto_extract_error": auto_extract_error,
            }),
        );

        // network_stores: opportunistic capture of content-bearing
        // fetch/XHR responses (JSON / GraphQL / NDJSON / route data).
        // Navigate result includes a SUMMARY (count + top-K metadata)
        // — full bodies are accessed via the network_stores RPC method
        // to keep the navigate result reasonable in size. Scoped to THIS
        // navigation_id so page A's captures don't leak into page B's
        // summary. (PR #7 review medium.)
        let network_stores = self._fetch.network_store.lock().ok().map(|s| {
            serde_json::to_value(s.summary(5, network_store::NavScope::Only(&nav_id)))
                .unwrap_or(Value::Null)
        });

        Ok(json!({
            "navigation_id": nav_id,
            "status": status,
            "url": final_url,
            "bytes": bytes,
            "headers": Value::Object(headers),
            "blockmap": blockmap,
            "challenge": challenge,
            "scripts": scripts,
            "extract": auto_extract,
            "network_stores": network_stores,
        }))
    }

    fn blockmap(&self) -> Result<Value> {
        self.eval("__blockmap()")
    }

    // Auto-strategy extraction. Tries JSON-LD → __NEXT_DATA__ → Nuxt →
    // OpenGraph/meta → microdata → text_main fallback, returns the
    // highest-confidence hit. Pass strategy="json_ld" etc. to force one.
    fn extract(&self, strategy: Option<&str>) -> Result<Value> {
        let opts = match strategy {
            Some(s) => format!("{{ strategy: {} }}", serde_json::to_string(s)?),
            None => "{}".to_string(),
        };
        self.eval(&format!("__extract({opts})"))
    }

    // Pull a <table> into {headers, rows, row_count}. Right tool for
    // pricing/specs/listings tables — saves the agent from writing a
    // querySelectorAll('tr') + per-cell mapping eval.
    fn extract_table(&self, selector: &str) -> Result<Value> {
        let sel_lit = serde_json::to_string(selector)?;
        self.eval(&format!("__extractTable({sel_lit})"))
    }

    // Pull a repeated card pattern into [{...}, ...]. `fields` maps field
    // name -> CSS sub-selector (with optional " @attr" suffix for an
    // attribute extraction). Right tool for HN-style lists, search results,
    // product grids — collapses per-site eval boilerplate to one call.
    fn extract_list(&self, item: &str, fields: &Value, limit: u32) -> Result<Value> {
        let item_lit = serde_json::to_string(item)?;
        let fields_lit = serde_json::to_string(fields)?;
        self.eval(&format!("__extractList({item_lit}, {fields_lit}, {limit})"))
    }

    // Drain the JS event loop: alternately runs queued microtasks (Promise
    // resolutions, queueMicrotask, etc.) and fires expired setTimeout/Interval
    // callbacks, sleeping to the next deadline when only timers remain.
    // Returns when the queue is fully empty OR `max_ms` elapses OR `max_iters`
    // iterations complete (whichever first).
    //
    // Iteration model:
    //   1. Drain all pending microtasks (via Runtime::execute_pending_job).
    //   2. Pump expired timers (JS-side __pumpTimers).
    //   3. If neither produced work and timers are pending, sleep to the
    //      earliest deadline (capped by remaining max_ms).
    //   4. If nothing is pending at all, exit.
    async fn settle(&self, max_ms: u64, max_iters: u32) -> Result<Value> {
        let start = std::time::Instant::now();
        let mut iters: u32 = 0;
        let mut total_microtasks: u64 = 0;
        let mut total_timers: u64 = 0;
        let mut total_fetches: u64 = 0;

        // Why settle exited. "idle" is the success case (all queues drained);
        // the others mean we hit a budget. Drivers can use this to pick a
        // failure mode: budget_exhausted suggests bumping max_ms or skipping
        // more scripts; max_iters suggests an infinite-microtask-loop
        // pattern (very rare in practice, but happens with libraries that
        // queueMicrotask in a loop). See PR review feedback / SPA proposal #6.
        // Filled in at every break path — clippy enforces no implicit default.
        let reason: &'static str;

        loop {
            if iters >= max_iters {
                reason = "max_iters";
                break;
            }
            let elapsed_ms = start.elapsed().as_millis() as u64;
            if elapsed_ms >= max_ms {
                reason = "budget_exhausted";
                break;
            }

            // 1. Drain microtasks. The inner loop honors max_ms — without
            // this, a MutationObserver→mutate→MutationObserver cascade (common
            // on hydrating Next.js / React SPAs like Vercel, Tailwind, Next.js
            // homepage) can run thousands of microtasks in a single outer
            // iter and blow past max_ms by 10–20×. Cap at 2000 microtasks per
            // pass as defense-in-depth (a normal page's burst is well under
            // a few hundred; 2k is 10× headroom).
            let mut mt_this_iter: u64 = 0;
            loop {
                let had_more = self
                    .js_rt
                    .execute_pending_job()
                    .map_err(|e| anyhow!("microtask exception: {e:?}"))?;
                if !had_more {
                    break;
                }
                mt_this_iter += 1;
                if mt_this_iter > 2_000 {
                    break;
                }
                if start.elapsed().as_millis() as u64 >= max_ms {
                    break;
                }
            }
            total_microtasks += mt_this_iter;

            // 2. Pump expired timers.
            let fired = self.eval("__pumpTimers()")?.as_u64().unwrap_or(0);
            total_timers += fired;

            // 3. Drain fetch responses (resolves pending Promises JS-side).
            // Note: pending_fetches covers BOTH JS-issued fetch()/XHR AND
            // dynamic-script loads from PR #6's __maybeHandleDynamicScript
            // (it routes through fetch). MutationObserver / IntersectionObserver
            // / ResizeObserver callbacks (PR #8) fire via queueMicrotask, so
            // they're covered by the microtask drain in step 1. We don't need
            // separate pending counters for those.
            let resolved = self.eval("__pollFetches()")?.as_u64().unwrap_or(0);
            total_fetches += resolved;

            // 4. Decide whether to keep going.
            let pending_timers = self.eval("__pendingTimers()")?.as_u64().unwrap_or(0);
            let pending_fetches = self.eval("__pendingFetches()")?.as_u64().unwrap_or(0);
            let microtasks_pending = self.js_rt.is_job_pending();

            if pending_timers == 0 && pending_fetches == 0 && !microtasks_pending {
                reason = "idle";
                break; // queue fully empty — the success case
            }

            let did_work_this_iter = mt_this_iter > 0 || fired > 0 || resolved > 0;
            if !did_work_this_iter && !microtasks_pending && pending_fetches > 0 {
                // Only fetches in flight — sleep briefly waiting for the worker
                // thread to push results.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            } else if !did_work_this_iter && !microtasks_pending && pending_timers > 0 {
                // Only timers are pending and none expired this iter — sleep
                // to the earliest deadline (capped by remaining time budget).
                let deadline = self.eval("__nextTimerDeadline()")?.as_f64();
                if let Some(deadline_ms) = deadline {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as f64)
                        .unwrap_or(0.0);
                    let remaining_budget = (max_ms.saturating_sub(elapsed_ms)) as f64;
                    let wait_ms = (deadline_ms - now_ms).max(0.0).min(remaining_budget);
                    if wait_ms > 0.0 {
                        tokio::time::sleep(std::time::Duration::from_millis(wait_ms as u64)).await;
                    }
                }
            }

            iters += 1;
        }

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(json!({
            "iters": iters,
            "elapsed_ms": elapsed_ms,
            "microtasks_run": total_microtasks,
            "timers_fired": total_timers,
            "fetches_resolved": total_fetches,
            "pending_timers": self.eval("__pendingTimers()")?.as_u64().unwrap_or(0),
            "pending_fetches": self.eval("__pendingFetches()")?.as_u64().unwrap_or(0),
            "pending_microtasks": self.js_rt.is_job_pending(),
            // Why settle exited. One of:
            //   "idle"             — all queues drained (success)
            //   "budget_exhausted" — wall-clock max_ms hit
            //   "max_iters"        — iteration cap hit (typically pathological microtask loop)
            // Drivers should use this to choose a recovery action.
            "reason": reason,
            // Back-compat: timed_out=true matches either budget_exhausted or
            // max_iters. New consumers should prefer `reason`. Kept so existing
            // drivers (and the `policy_trace` consumers) don't break. Drop in
            // a follow-up once everyone's migrated.
            "timed_out": reason != "idle",
        }))
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

    // Find elements by visible text content, skipping chrome (header/nav/
    // footer/aside/script/style). Returns the smallest/deepest element
    // whose textContent matches the needle. Anchor-promotion: if the deepest
    // match is a <span>/<strong>/etc. whose direct parent is <a>, the anchor
    // is returned instead (so click() targets the actionable element).
    //
    // Right tool for sites where CSS selectors are unstable (React-rendered
    // pages with hashed class names) but the visible text is reliable.
    fn query_text(
        &self,
        text: &str,
        selector: Option<&str>,
        exact: bool,
        limit: u32,
    ) -> Result<Value> {
        let text_lit = serde_json::to_string(text)?;
        let sel_lit = match selector {
            Some(s) => serde_json::to_string(s)?,
            None => "null".to_string(),
        };
        let code = format!(
            r#"(function(){{
                var needle = {text_lit};
                var sel = {sel_lit};
                var exact = {exact};
                var limit = {limit};
                var lowerNeedle = needle.toLowerCase();
                function clean(s) {{ return (s || '').replace(/\s+/g, ' ').trim(); }}
                function isChromeTag(t) {{
                    return t === 'header' || t === 'nav' || t === 'footer' ||
                           t === 'aside' || t === 'script' || t === 'style' ||
                           t === 'noscript';
                }}
                // Pre-filter (descent gate): always substring — we need to
                // recurse if any descendant might match, regardless of mode.
                function contains(t) {{
                    return clean(t).toLowerCase().indexOf(lowerNeedle) !== -1;
                }}
                // Final match test (decides whether to push this node):
                // exact requires equality, otherwise substring is enough.
                function isMatch(t) {{
                    var c = clean(t);
                    return exact ? (c === needle) : (c.toLowerCase().indexOf(lowerNeedle) !== -1);
                }}
                var hits = [];
                function visit(node) {{
                    if (hits.length >= limit) return;
                    if (!node || node.nodeType !== 1) return;
                    var tag = node.tagName.toLowerCase();
                    if (isChromeTag(tag)) return;
                    var text = node.textContent || '';
                    if (!contains(text)) return;
                    var beforeCount = hits.length;
                    for (var i = 0; i < node.childNodes.length; i++) {{
                        visit(node.childNodes[i]);
                        if (hits.length >= limit) return;
                    }}
                    if (hits.length === beforeCount && isMatch(text)) {{
                        var target = node;
                        if (node.parentNode && node.parentNode.tagName === 'A' &&
                            ['SPAN','STRONG','EM','B','I','SMALL','MARK'].indexOf(node.tagName) !== -1) {{
                            target = node.parentNode;
                        }}
                        hits.push(target);
                    }}
                }}
                var roots;
                if (sel) {{
                    var nodeList = document.querySelectorAll(sel);
                    roots = [];
                    for (var i = 0; i < nodeList.length; i++) roots.push(nodeList[i]);
                }} else {{
                    roots = [document.body];
                }}
                for (var i = 0; i < roots.length; i++) visit(roots[i]);
                return hits.map(function(el) {{
                    return {{
                        ref: 'e:' + el._id,
                        tag: el.tagName.toLowerCase(),
                        attrs: el._attributes,
                        text: clean(el.textContent).slice(0, 200),
                    }};
                }});
            }})()"#
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
            // If the resolved target is a known tracker URL (Bing/Google/DDG
            // result-link wrapper), decode to the real destination so we
            // don't land on the tracker's JS-redirect shell.
            let target = decode_tracker(&target).unwrap_or(target);
            return self.navigate(&target, false).await;
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
        let enctype = info
            .get("enctype")
            .and_then(|v| v.as_str())
            .unwrap_or("application/x-www-form-urlencoded");
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

        let target_url = self.resolve_url(action)?;

        match method {
            "get" => {
                let mut target =
                    url::Url::parse(&target_url).map_err(|e| anyhow!("resolve action url: {e}"))?;
                {
                    let mut qp = target.query_pairs_mut();
                    qp.clear();
                    for (n, v) in &pairs {
                        qp.append_pair(n, v);
                    }
                }
                self.navigate(target.as_str(), false).await
            }
            "post" => {
                if !enctype.starts_with("application/x-www-form-urlencoded") {
                    // multipart/form-data needs a different request shape
                    // (boundary, Content-Type, per-part headers). Defer until
                    // there's a real use case to model the surface against.
                    return Err(anyhow!(
                        "POST enctype '{enctype}' not supported (only application/x-www-form-urlencoded)"
                    ));
                }
                let body = url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(pairs.iter().map(|(n, v)| (n.as_str(), v.as_str())))
                    .finish();
                let req = self
                    .http
                    .post(&target_url)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(body);
                self.navigate_with(req, false).await
            }
            other => Err(anyhow!("unsupported form method '{other}'")),
        }
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

// Search engines wrap result links in tracker URLs (so they can record
// click-throughs) that the destination's own server never sees. When an
// agent click-follows one of these, the right behavior is to land on the
// real destination, not the tracker page — which is often a JS-redirect
// shell our static fetch can't actually follow.
//
// Returns Some(decoded_url) for known tracker shapes, None otherwise.
// Caller (click follow) substitutes the decoded URL when present.
fn decode_tracker(href: &str) -> Option<String> {
    use base64::Engine;
    let parsed = url::Url::parse(href).ok()?;
    let host = parsed.host_str()?;

    // Bing — bing.com/ck/a?...&u=a1<urlsafe-base64>&...
    // The 'a1' prefix is Bing's "this is a base64-encoded URL" marker.
    if host.ends_with("bing.com") && parsed.path() == "/ck/a" {
        let u = parsed.query_pairs().find(|(k, _)| k == "u")?.1;
        let payload = u.strip_prefix("a1").unwrap_or(&u);
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload.as_bytes())
            .ok()?;
        return String::from_utf8(bytes).ok();
    }

    // Google — google.com/url?q=<urlencoded>&...
    // The url crate's query_pairs() already URL-decodes for us.
    if host.ends_with("google.com") && parsed.path() == "/url" {
        return parsed
            .query_pairs()
            .find(|(k, _)| k == "q")
            .map(|(_, v)| v.into_owned());
    }

    // DuckDuckGo HTML version — duckduckgo.com/l/?uddg=<urlencoded>&...
    if host.ends_with("duckduckgo.com") && parsed.path() == "/l/" {
        return parsed
            .query_pairs()
            .find(|(k, _)| k == "uddg")
            .map(|(_, v)| v.into_owned());
    }

    None
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
                // Newer wording (post-2024 Turnstile/managed challenge):
                "verifying you are human",
                "needs to review the security of your connection",
                "performance & security by cloudflare",
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
                "verify that you're human",
                "please wait for verification",
                "please wait while we verify",
                "unusual traffic",
                "access to this page has been denied",
                "access denied",
                "automated requests",
                "sorry, you have been blocked",
                "you are being rate limited",
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

// One <script> element from a parsed page.
//
// `kind` distinguishes async from non-async. Inline and Defer are treated as
// Sync for execution because we don't have incremental HTML parsing — by the
// time scripts run, the document is fully parsed, so "execute after parse in
// document order" (Defer's spec semantics) and "execute now in document order"
// (Sync) collapse to the same thing for us. Only `async` differs: async
// scripts may execute out of document order (we run them after the sync
// queue, in fetch-completion order).
//
// Inline scripts cannot be async (browsers ignore the attribute on inline),
// so we don't track kind for Inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptKind {
    Sync,
    Async,
}

enum ScriptItem {
    Inline(String),
    External { url: String, kind: ScriptKind },
}

// Heuristic: does this source look like an ES module? Checks the first few
// lines for `import` / `export` statements. Used by the static script-eval
// loop to route module-shaped sources through __loadModule (which fetches
// deps then evals) vs plain eval (which would throw SyntaxError on the
// import keyword and dispatch script_executed{error}).
//
// Conservative — false negatives just go through plain eval (and fail
// loudly via PR #6's eval-error path); false positives go through the
// module loader (which strips imports + evals — equivalent to plain eval
// for source with no actual imports). No correctness loss either way.
fn looks_like_module(source: &str) -> bool {
    for line in source.lines().take(50) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("import ")
            || trimmed.starts_with("import{")
            || trimmed.starts_with("import\"")
            || trimmed.starts_with("import'")
            || trimmed.starts_with("import*")
            || trimmed.starts_with("export ")
            || trimmed.starts_with("export{")
            || trimmed.starts_with("export*")
            || trimmed.starts_with("export default")
        {
            return true;
        }
    }
    false
}

// Walk the parsed HTML tree and collect <script> elements in document order.
// Skips:
//   - <script type="application/json"> (data, not code — accessible via eval)
//   - <script type="application/ld+json"> (structured data)
//   - any non-empty `type` other than text/javascript or module
// External srcs resolved against `base_url`; ones that fail to resolve are dropped.
fn collect_scripts(tree: &Value, base_url: &str) -> Vec<ScriptItem> {
    let mut out = Vec::new();
    let base = url::Url::parse(base_url).ok();
    walk_for_scripts(tree, base.as_ref(), &mut out);
    out
}

fn walk_for_scripts(node: &Value, base: Option<&url::Url>, out: &mut Vec<ScriptItem>) {
    let Some(obj) = node.as_object() else {
        return;
    };
    let is_element = obj.get("type").and_then(|t| t.as_str()) == Some("element");
    let tag = obj.get("tag").and_then(|t| t.as_str()).unwrap_or("");
    if is_element && tag == "script" {
        let attrs = obj.get("attrs").and_then(|a| a.as_object());
        let src = attrs.and_then(|a| a.get("src")).and_then(|v| v.as_str());
        let ty = attrs
            .and_then(|a| a.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let is_js = ty.is_empty()
            || ty.eq_ignore_ascii_case("module")
            || ty.to_ascii_lowercase().contains("javascript");
        if is_js {
            if let Some(src_url) = src {
                if !src_url.is_empty()
                    && let Some(b) = base
                    && let Ok(resolved) = b.join(src_url)
                {
                    // HTML treats `async` and `defer` as boolean attrs — any
                    // presence (even empty value) counts. `async` wins if both
                    // are set, matching browsers.
                    let is_async = attrs.and_then(|a| a.get("async")).is_some();
                    let kind = if is_async {
                        ScriptKind::Async
                    } else {
                        ScriptKind::Sync
                    };
                    out.push(ScriptItem::External {
                        url: resolved.to_string(),
                        kind,
                    });
                }
            } else if let Some(children) = obj.get("children").and_then(|c| c.as_array()) {
                let mut content = String::new();
                for child in children {
                    if let Some(cobj) = child.as_object()
                        && cobj.get("type").and_then(|t| t.as_str()) == Some("text")
                        && let Some(text) = cobj.get("content").and_then(|t| t.as_str())
                    {
                        content.push_str(text);
                    }
                }
                if !content.trim().is_empty() {
                    out.push(ScriptItem::Inline(content));
                }
            }
        }
    }
    if let Some(children) = obj.get("children").and_then(|c| c.as_array()) {
        for child in children {
            walk_for_scripts(child, base, out);
        }
    }
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

// Parse an HTML fragment (e.g. the rhs of `el.innerHTML = '<p>...</p>'`).
// Context element is <body> — matches what real browsers do for innerHTML
// on most elements. (Tables and selects use different contexts; v1 punts
// on those — they parse OK under <body> in practice for typical uses.)
//
// Returns a JSON string with the shape {type: "element", tag: "fragment",
// attrs: {}, children: [...]} where each child matches the format from
// parse_html_to_tree. Caller is JS-side __parseHTMLFragment() in
// dom.js — it JSON.parses the string and feeds children to buildChildren.
//
// Why JSON-string instead of constructing a JS object: avoids reaching
// across the rquickjs binding boundary to build nested objects, which
// is significantly more lines and harder to maintain than a JSON dance.
fn parse_html_fragment_to_json(html: &str) -> String {
    use html5ever::interface::QualName;
    use html5ever::{local_name, ns};

    let context = QualName::new(None, ns!(html), local_name!("body"));
    let dom = html5ever::parse_fragment(
        RcDom::default(),
        Default::default(),
        context,
        Vec::new(),
        false,
    )
    .from_utf8()
    .read_from(&mut html.as_bytes())
    .unwrap_or_else(|_| RcDom::default());

    // parse_fragment produces a synthetic context element under
    // dom.document — its children are the actual fragment.
    let mut children: Vec<Value> = Vec::new();
    for ctx_child in dom.document.children.borrow().iter() {
        if matches!(&ctx_child.data, NodeData::Element { .. }) {
            for inner in ctx_child.children.borrow().iter() {
                if let Some(v) = child_to_json(inner) {
                    children.push(v);
                }
            }
            break;
        }
    }
    let tree = json!({
        "type": "element",
        "tag": "fragment",
        "attrs": {},
        "children": children,
    });
    serde_json::to_string(&tree).unwrap_or_else(|_| "{}".to_string())
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

// Lowercased host extracted from a URL; "" on parse failure or hostless.
// Used by script_decision events. Centralized so the event shape stays
// consistent across the first-pass and assembly-pass emissions.
fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_lowercase()))
        .unwrap_or_default()
}

fn script_kind_str(kind: ScriptKind) -> &'static str {
    match kind {
        ScriptKind::Sync => "sync",
        ScriptKind::Async => "async",
    }
}

// Phase A: validated outcome reporting. Shared between rpc_main and
// dispatch_tool so the validation and event shape stay canonical. v0
// just emits the NDJSON event — no posterior updates yet (see
// docs/probabilistic-policy.md §4.5).
//
// Returns Err with a human-readable message on schema violations.
// Unknown nav_id is rejected so an outcome can never silently corrupt
// future posterior attribution.
const TASK_CLASS_ENUM: &[&str] = &["extract", "query", "click", "form", "visual"];

fn validate_and_emit_outcome(
    session: &Session,
    params: &Value,
    nav_id: &str,
) -> std::result::Result<(), String> {
    if nav_id.is_empty() {
        return Err("missing 'navigation_id' param".to_string());
    }
    if !session.nav_id_is_known(nav_id) {
        return Err(format!(
            "unknown navigation_id '{nav_id}' — never issued by this session"
        ));
    }
    // success is required by the schema. Missing → reject (don't default to false).
    let success = match params.get("success") {
        Some(v) => v
            .as_bool()
            .ok_or_else(|| "'success' must be boolean".to_string())?,
        None => return Err("missing required 'success' param".to_string()),
    };
    let task_class = match params.get("task_class") {
        Some(Value::Null) | None => None,
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| "'task_class' must be string".to_string())?;
            if !TASK_CLASS_ENUM.contains(&s) {
                return Err(format!(
                    "'task_class' must be one of {TASK_CLASS_ENUM:?}, got '{s}'"
                ));
            }
            Some(s)
        }
    };
    let task_id = params.get("task_id").and_then(|v| v.as_str());
    let quality = params.get("quality").and_then(|v| v.as_f64());
    let error = params.get("error").and_then(|v| v.as_str());
    emit_event(
        "outcome_reported",
        json!({
            "schema_version": 1,
            "navigation_id": nav_id,
            "task_id": task_id,
            "task_class": task_class,
            "success": success,
            "quality": quality,
            "error": error,
        }),
    );
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--list-profiles") {
        for n in profile::Profile::list_builtin() {
            println!("{n}");
        }
        return Ok(());
    }
    if args.iter().any(|a| a == "--prefit-info") {
        match prefit::PrefitBundle::load_embedded() {
            Some(b) => {
                println!("schema_version: {}", b.schema_version);
                println!("training_pipeline_version: {}", b.training_pipeline_version);
                println!("fit_timestamp: {}", b.fit_timestamp);
                println!("fit_corpus_size: {}", b.fit_corpus_size);
                println!("domains: {}", b.domain_count());
                let mut keys: Vec<_> = b.domains.keys().collect();
                keys.sort();
                for k in keys {
                    if let Some(d) = b.domains.get(k) {
                        println!(
                            "  {:30} framework={:20} blocklist_additions={:3} shape={}",
                            k,
                            d.framework.as_deref().unwrap_or("-"),
                            d.blocklist_additions.len(),
                            d.shape_hint.as_deref().unwrap_or("-")
                        );
                    }
                }
                return Ok(());
            }
            None => {
                eprintln!("prefit: failed to load embedded bundle");
                std::process::exit(2);
            }
        }
    }
    if args.get(1).map(|s| s.as_str()) == Some("policy-check") {
        return policy_check_cmd(&args[2..]);
    }
    let profile_name = parse_profile_arg(&args);
    let profile = Profile::load(&profile_name)?;
    if args.iter().any(|a| a == "--mcp") {
        mcp_main(profile).await
    } else {
        rpc_main(profile).await
    }
}

// `unbrowser policy-check <url> [<url>...]`
//
// Prints the policy decision for one or more URLs. Used to verify the
// blocklist against ad-hoc URLs and to drive scripts/policy_baseline.py
// without round-tripping through navigate. Pure stdlib + policy module —
// no JS engine, no HTTP.
fn policy_check_cmd(urls: &[String]) -> Result<()> {
    if urls.is_empty() {
        eprintln!("usage: unbrowser policy-check <url> [<url>...]");
        eprintln!("       unbrowser policy-check --info");
        std::process::exit(2);
    }
    if urls.iter().any(|u| u == "--info") {
        println!("entries: {}", policy::entry_count());
        return Ok(());
    }
    for url in urls {
        let d = policy::decide(url);
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "<unparsed>".to_string());
        if d.blocked {
            println!(
                "block\t{}\t{}\t{}\t{}",
                d.category.map(|c| c.as_str()).unwrap_or("?"),
                d.matched_pattern.unwrap_or("?"),
                host,
                url
            );
        } else {
            println!("allow\t-\t-\t{}\t{}", host, url);
        }
    }
    Ok(())
}

// `--profile <name>` or `--profile=<name>`. Falls back to UNBROWSER_PROFILE
// env var, then the built-in default.
fn parse_profile_arg(args: &[String]) -> String {
    for (i, a) in args.iter().enumerate() {
        if a == "--profile" {
            if let Some(next) = args.get(i + 1) {
                return next.clone();
            }
        } else if let Some(rest) = a.strip_prefix("--profile=") {
            return rest.to_string();
        }
    }
    std::env::var("UNBROWSER_PROFILE").unwrap_or_else(|_| profile::DEFAULT_PROFILE.to_string())
}

// `--policy=blocklist` enables Tier 1 deterministic blocking at the
// `__host_fetch_send` layer. Off by default — opt-in for v0 until the
// corpus measurement validates no extraction-quality regression. Env var
// UNBROWSER_POLICY=blocklist also flips it on for ad-hoc shell use.
fn parse_policy_arg(args: &[String]) -> bool {
    if args
        .iter()
        .any(|a| a == "--policy=blocklist" || a == "--policy=on")
    {
        return true;
    }
    std::env::var("UNBROWSER_POLICY")
        .map(|v| v == "blocklist" || v == "on")
        .unwrap_or(false)
}

// Per-RPC wall-clock budget for JS eval. Default 30s — fits the watchdog
// design rationale (script phase tightens to 5s, settle gets the remainder).
// Sites with legitimately slow SSR/hydration can set UNBROWSER_TIMEOUT_MS
// higher; clamped to [1000, 600_000] (1s..10min) to keep silly values from
// re-introducing the orphan-leak class of bug.
fn read_dispatch_budget_ms() -> u64 {
    std::env::var("UNBROWSER_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|v| v.clamp(1_000, 600_000))
        .unwrap_or(30_000)
}

async fn rpc_main(profile: Profile) -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let policy_block = parse_policy_arg(&args);
    let profile_name = profile.name.clone();
    let mut session = Session::new(&profile, policy_block)?;
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let dispatch_budget_ms = read_dispatch_budget_ms();
    emit_event(
        "ready",
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "dispatch_budget_ms": dispatch_budget_ms,
            "profile": profile_name,
        }),
    );

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
        // Bound EVERY RPC call's JS work with a wall-clock deadline. The
        // watchdog interrupt handler installed in Session::new aborts any
        // running eval (script-phase, settle pump, microtask, query) once
        // the deadline passes. Without this, exec_scripts=true on hostile
        // SPAs left CPU-pegged orphan processes behind. Restore on the way
        // out so back-to-back calls each get a fresh budget. Default 30s,
        // tunable via UNBROWSER_TIMEOUT_MS for legit-but-slow sites.
        let prev_dispatch_deadline = session.set_eval_deadline_from_now(dispatch_budget_ms);
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
                Some(u) => {
                    let exec = req
                        .params
                        .get("exec_scripts")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    match session.navigate(u, exec).await {
                        Ok(v) => ok_response(id, v),
                        Err(e) => err_response(id, -2, e.to_string()),
                    }
                }
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
            "query_text" => {
                let text = req.params.get("text").and_then(|v| v.as_str());
                let selector = req.params.get("selector").and_then(|v| v.as_str());
                let exact = req
                    .params
                    .get("exact")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let limit = req
                    .params
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20) as u32;
                match text {
                    Some(t) => match session.query_text(t, selector, exact, limit) {
                        Ok(v) => ok_response(id, v),
                        Err(e) => err_response(id, -5, e.to_string()),
                    },
                    None => err_response(id, -32602, "missing 'text' param"),
                }
            }
            "blockmap" => match session.blockmap() {
                Ok(v) => ok_response(id, v),
                Err(e) => err_response(id, -6, e.to_string()),
            },
            "extract" => {
                let strategy = req.params.get("strategy").and_then(|v| v.as_str());
                match session.extract(strategy) {
                    Ok(v) => ok_response(id, v),
                    Err(e) => err_response(id, -6, e.to_string()),
                }
            }
            "extract_table" => match req.params.get("selector").and_then(|v| v.as_str()) {
                Some(s) => match session.extract_table(s) {
                    Ok(v) => ok_response(id, v),
                    Err(e) => err_response(id, -6, e.to_string()),
                },
                None => err_response(id, -32602, "missing 'selector' param"),
            },
            "extract_list" => {
                let item = req.params.get("item_selector").and_then(|v| v.as_str());
                let fields = req.params.get("fields");
                let limit = req
                    .params
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1000) as u32;
                match (item, fields) {
                    (Some(i), Some(f)) => match session.extract_list(i, f, limit) {
                        Ok(v) => ok_response(id, v),
                        Err(e) => err_response(id, -6, e.to_string()),
                    },
                    _ => err_response(id, -32602, "missing 'item_selector' or 'fields' param"),
                }
            }
            "settle" => {
                let max_ms = req
                    .params
                    .get("max_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(2000);
                let max_iters = req
                    .params
                    .get("max_iters")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50) as u32;
                match session.settle(max_ms, max_iters).await {
                    Ok(v) => ok_response(id, v),
                    Err(e) => err_response(id, -6, e.to_string()),
                }
            }
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
            "report_outcome" => {
                let nav_id = req
                    .params
                    .get("navigation_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match validate_and_emit_outcome(&session, &req.params, &nav_id) {
                    Ok(()) => ok_response(id, json!({ "ok": true })),
                    Err(msg) => err_response(id, -32602, msg),
                }
            }
            "network_stores" => {
                let limit = req
                    .params
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20) as usize;
                let host = req.params.get("host").and_then(|v| v.as_str());
                // nav_id default: most recent (i.e. current) navigation.
                // "all" → no nav filter. Explicit "nav_<n>" → that one only.
                // (PR #7 review medium: prevent stale page-A data.)
                let nav_param = req.params.get("nav_id").and_then(|v| v.as_str());
                let scope_id: Option<String> = match nav_param {
                    Some("all") => None,
                    Some(explicit) => Some(explicit.to_string()),
                    None => session
                        ._fetch
                        .current_nav_id
                        .lock()
                        .ok()
                        .and_then(|g| g.clone()),
                };
                let scope = match scope_id.as_deref() {
                    Some(id) => network_store::NavScope::Only(id),
                    None => network_store::NavScope::All,
                };
                let captures = session
                    ._fetch
                    .network_store
                    .lock()
                    .map(|s| s.ranked(limit, host, scope))
                    .unwrap_or_default();
                ok_response(id, serde_json::to_value(&captures).unwrap_or(Value::Null))
            }
            "network_stores_clear" => {
                if let Ok(mut s) = session._fetch.network_store.lock() {
                    s.clear();
                }
                ok_response(id, json!({ "ok": true }))
            }
            "close" => {
                write_response(&mut out, &ok_response(id, json!("bye")))?;
                return Ok(());
            }
            other => err_response(id, -32601, format!("unknown method: {other}")),
        };
        session.restore_eval_deadline(prev_dispatch_deadline);
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
            "description": "Fetch a URL with Chrome-fingerprinted HTTP (rquest, Chrome 131 emulation). Parses HTML, seeds the JS DOM, returns BlockMap inline. With `exec_scripts: true`, extracts inline AND external <script> tags from the parsed HTML, fetches externals in parallel (8s per-fetch timeout), eval's them in document order in QuickJS (with shims for setTimeout/fetch/etc.), then settles the event loop and fires DOMContentLoaded + load. `<script async>` is honored: async scripts execute after the sync queue. When `--policy=blocklist` is set, tracker URLs are blocked at script-fetch time (see scripts.policy_blocked in the result). Returns a `scripts` summary with inline_count, external_count, async_count, policy_blocked, executed, errors.\n\nAuto-extract: when the page embeds JSON-bearing <script> tags (density.json_scripts > 0 — covers application/json, application/ld+json, text/x-magento-init, text/x-shopify-app, etc.), navigate auto-runs `extract()` and returns the result as the `extract` field. Saves a round trip on the common case where the data the JS would have rendered is already sitting in the HTML — JSON-LD article schemas on news sites, __NEXT_DATA__ page state on Next.js apps, json_in_script product blobs on Magento/Shopify, GitHub RSC payloads, etc. Capped at 256 KB inline; over that limit `extract` returns a stub with strategy/confidence/size_bytes/hint and the agent should call `extract()` explicitly to retrieve the full payload. Pages with no embedded JSON get extract:null and pay zero extra cost.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url":          { "type": "string", "description": "Absolute URL to fetch" },
                    "exec_scripts": { "type": "boolean", "description": "Run page <script> tags (inline + external src) after parse, settle the event loop, and fire DOMContentLoaded + load. Default false." }
                },
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
            "name": "query_text",
            "description": "Find elements by visible text content. Returns the smallest/deepest element whose textContent matches the needle, with chrome (header/nav/footer/aside) skipped. Anchor-promotion: a span/strong/etc. inside an <a> resolves to the anchor (so click() targets the actionable element). Right tool when CSS selectors are unstable (React-rendered pages with hashed class names) but the visible label is reliable — e.g. find a 'Sign in' button without knowing its class.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text":     { "type": "string", "description": "Substring to match (or exact string if exact=true)" },
                    "selector": { "type": "string", "description": "Optional CSS selector to limit search scope (default: whole document body)" },
                    "exact":    { "type": "boolean", "description": "If true, exact match instead of substring (default false)" },
                    "limit":    { "type": "integer", "description": "Max matches to return (default 20)" }
                },
                "required": ["text"]
            }
        },
        {
            "name": "blockmap",
            "description": "Recompute the BlockMap for the current page. Use after eval'd JS or click/type modifies the DOM. Same shape as the inline blockmap from navigate.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "extract",
            "description": "Auto-strategy structured-data extraction. Tries JSON-LD (schema.org) → __NEXT_DATA__ → Nuxt → JSON-in-script (Magento, Shopify, BigCommerce custom-typed scripts) → OpenGraph/meta → microdata → text_main fallback, returns the highest-confidence hit as {strategy, confidence, data, tried}. Use this as the one-shot 'give me the data, you figure out how' call when you don't want to plan the strategy yourself. Pass strategy='json_ld' (or any of the names above) to force a specific extractor.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "strategy": { "type": "string", "description": "Optional: force a specific extractor (json_ld, next_data, nuxt_data, json_in_script, og_meta, microdata, text_main)" }
                }
            }
        },
        {
            "name": "extract_table",
            "description": "Pull a <table> into {headers, rows, row_count}. Headers come from <thead><th>...</th></thead> if present, else the first <tr>'s <th> cells. Each subsequent <tr>'s <td> cells become a row dict keyed by header (or 'col_N' if no header for that column). Right tool for pricing tables, specs, finance/listings tables — saves writing the per-cell mapping eval.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector matching the <table> element" }
                },
                "required": ["selector"]
            }
        },
        {
            "name": "extract_list",
            "description": "Pull a repeated card pattern into [{...}, {...}]. Right tool for HN-style lists, search results, product grids — collapses per-site eval boilerplate. Field spec shapes: 'css selector' (text content), 'css selector @attr' (attribute), or ['css selector', '@attr'] (tuple form). If a sub-selector returns null, the field value is null.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "item_selector": { "type": "string", "description": "CSS selector matching each card/row" },
                    "fields": { "type": "object", "description": "{field_name: 'sub-selector' | 'sub-selector @attr' | ['sub-selector', '@attr']}" },
                    "limit": { "type": "integer", "description": "Max items to extract (default 1000)" }
                },
                "required": ["item_selector", "fields"]
            }
        },
        {
            "name": "settle",
            "description": "Drain the JS event loop: alternately runs queued microtasks (Promise resolutions) and fires expired setTimeout/setInterval callbacks, sleeping to the next deadline when only timers remain. Returns when the queue is empty OR max_ms elapses OR max_iters iterations complete. Defaults: max_ms=2000, max_iters=50. Use after seeding the DOM (or after eval'd code that schedules timers) to let pending callbacks run.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_ms":    { "type": "integer", "description": "Max wall-clock ms to spend (default 2000)" },
                    "max_iters": { "type": "integer", "description": "Max iterations of the drain loop (default 50)" }
                }
            }
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
        },
        {
            "name": "report_outcome",
            "description": "Bind a task outcome (success/failure/quality) to a previous navigation_id from a navigate() call. Used by the policy framework's outcome protocol — see docs/probabilistic-policy.md §4.5. v0 emits an outcome_reported NDJSON event for the navigation; no posterior updates yet. Drivers should call this once per agent task so future Bayesian phases (B/D-2) can attribute extraction success/failure to specific policy decisions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "navigation_id": { "type": "string", "description": "The id returned by navigate() — joins this outcome to the policy_trace event." },
                    "task_id":       { "type": "string", "description": "Optional opaque id chosen by the driver for cross-system correlation." },
                    "task_class":    { "type": "string", "enum": ["extract", "query", "click", "form", "visual"], "description": "What kind of task succeeded/failed. Lets future posteriors condition on task class." },
                    "success":       { "type": "boolean", "description": "Did the agent's task succeed?" },
                    "quality":       { "type": "number", "description": "Optional 0..1 quality score (e.g. fraction of expected fields extracted)." },
                    "error":         { "type": "string", "description": "Optional human-readable error/explanation when success=false." }
                },
                "required": ["navigation_id", "success"]
            }
        },
        {
            "name": "network_stores",
            "description": "Return content-bearing fetch/XHR responses captured during navigate, ranked by likely content value. SPAs often keep their data in API responses (JSON, GraphQL, NDJSON, Next/Nuxt route data) that are cleaner than the rendered DOM — this tool surfaces them directly. Each entry has capture_id, URL, status, content-type, body_preview (truncated to 256 KB), body_bytes (full size), body_truncated flag, navigation_id, and a heuristic score. Bodies for trackers/ads/CSS/HTML/media are NOT captured. The navigate result already contains a top-5 summary scoped to that navigation; use this tool to get more entries, filter by host, or pull captures from a different navigation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit":  { "type": "integer", "description": "Max entries to return (default 20).", "minimum": 1, "maximum": 100 },
                    "host":   { "type": "string", "description": "Optional substring filter on response host." },
                    "nav_id": { "type": "string", "description": "Defaults to the most recent navigation_id (page B never sees page A captures). Pass an explicit navigation_id from a prior navigate result to query that navigation specifically. Pass 'all' to disable nav filtering and return captures from every navigation." }
                }
            }
        },
        {
            "name": "network_stores_clear",
            "description": "Drop all captured network responses from the session's network store. Use this between unrelated navigations if you don't want earlier captures showing up in later network_stores calls.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

async fn dispatch_tool(session: &mut Session, name: &str, args: &Value) -> Result<Value> {
    let str_arg = |k: &str| args.get(k).and_then(|v| v.as_str());
    match name {
        "navigate" => {
            let url = str_arg("url").ok_or_else(|| anyhow!("missing 'url'"))?;
            let exec = args
                .get("exec_scripts")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            session.navigate(url, exec).await
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
        "query_text" => {
            let text = str_arg("text").ok_or_else(|| anyhow!("missing 'text'"))?;
            let selector = str_arg("selector");
            let exact = args.get("exact").and_then(|v| v.as_bool()).unwrap_or(false);
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as u32;
            session.query_text(text, selector, exact, limit)
        }
        "blockmap" => session.blockmap(),
        "extract" => {
            let strategy = str_arg("strategy");
            session.extract(strategy)
        }
        "extract_table" => {
            let sel = str_arg("selector").ok_or_else(|| anyhow!("missing 'selector'"))?;
            session.extract_table(sel)
        }
        "extract_list" => {
            let item =
                str_arg("item_selector").ok_or_else(|| anyhow!("missing 'item_selector'"))?;
            let fields = args
                .get("fields")
                .ok_or_else(|| anyhow!("missing 'fields'"))?;
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(1000) as u32;
            session.extract_list(item, fields, limit)
        }
        "settle" => {
            let max_ms = args.get("max_ms").and_then(|v| v.as_u64()).unwrap_or(2000);
            let max_iters = args.get("max_iters").and_then(|v| v.as_u64()).unwrap_or(50) as u32;
            session.settle(max_ms, max_iters).await
        }
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
        "report_outcome" => {
            let nav_id =
                str_arg("navigation_id").ok_or_else(|| anyhow!("missing 'navigation_id'"))?;
            validate_and_emit_outcome(session, args, nav_id).map_err(|e| anyhow!(e))?;
            Ok(json!({ "ok": true }))
        }
        "network_stores" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            let host = str_arg("host");
            let nav_param = str_arg("nav_id");
            let scope_id: Option<String> = match nav_param {
                Some("all") => None,
                Some(explicit) => Some(explicit.to_string()),
                None => session
                    ._fetch
                    .current_nav_id
                    .lock()
                    .ok()
                    .and_then(|g| g.clone()),
            };
            let scope = match scope_id.as_deref() {
                Some(id) => network_store::NavScope::Only(id),
                None => network_store::NavScope::All,
            };
            let captures = session
                ._fetch
                .network_store
                .lock()
                .map(|s| s.ranked(limit, host, scope))
                .unwrap_or_default();
            Ok(serde_json::to_value(&captures).unwrap_or(Value::Null))
        }
        "network_stores_clear" => {
            if let Ok(mut s) = session._fetch.network_store.lock() {
                s.clear();
            }
            Ok(json!({ "ok": true }))
        }
        _ => Err(anyhow!("unknown tool: {name}")),
    }
}

async fn mcp_main(profile: Profile) -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let policy_block = parse_policy_arg(&args);
    let mut session = Session::new(&profile, policy_block)?;
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let dispatch_budget_ms = read_dispatch_budget_ms();

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
                    "name": "unbrowser",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": mcp_tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
                // Same watchdog budget as the bare-RPC dispatcher.
                let prev = session.set_eval_deadline_from_now(dispatch_budget_ms);
                let outcome = dispatch_tool(&mut session, name, &arguments).await;
                session.restore_eval_deadline(prev);
                match outcome {
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
