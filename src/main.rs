use std::collections::HashMap;
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
}

fn spawn_fetch_worker(http: rquest::Client) -> FetchQueue {
    let (tx, rx) = mpsc::channel::<FetchRequest>();
    let results: Arc<Mutex<Vec<FetchResponse>>> = Arc::new(Mutex::new(Vec::new()));
    let results_for_thread = results.clone();

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
                    let resp = run_fetch(http.clone(), req).await;
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
}

impl Session {
    fn new(profile: &Profile) -> Result<Self> {
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
            let sender = fetch.sender.clone();
            let host_send = rquickjs::Function::new(
                ctx.clone(),
                move |id: f64, method: String, url: String, headers_json: String, body: String| {
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

            Ok(())
        })?;
        Ok(Self {
            js_rt,
            js_ctx,
            http,
            jar,
            _fetch: fetch,
            eval_deadline_ms,
            last_url: None,
            last_body: None,
        })
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
        let resp = req.send().await.context("http send")?;
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
        if let Some(c) = &challenge {
            emit_event("challenge", c.clone());
        }

        let tree = parse_html_to_tree(&body);
        self.seed_dom(&tree)?;

        // Update window.location for any page scripts that read it.
        let url_lit = serde_json::to_string(&final_url)?;
        let _ = self.eval(&format!("__setLocation({url_lit})"));

        // Phase 5: optionally execute page scripts (inline + external src).
        let scripts = if exec_scripts && (200..400).contains(&status) {
            let items = collect_scripts(&tree, &final_url);
            let mut inline_count = 0usize;
            let mut external_count = 0usize;
            let mut fetch_errors: Vec<String> = Vec::new();

            // Spawn external fetches in parallel — current_thread runtime
            // interleaves them at network-I/O await points, so a page with
            // N external bundles takes ~max(round-trip times) instead of
            // sum(round-trip times). Each task has a per-fetch timeout so
            // a single huge bundle can't hang the navigate indefinitely.
            // Document ordering preserved by indexing results.
            const SCRIPT_FETCH_TIMEOUT_MS: u64 = 8000;
            let mut fetch_tasks: Vec<(usize, tokio::task::JoinHandle<Result<String, String>>)> =
                Vec::new();
            for (idx, item) in items.iter().enumerate() {
                if let ScriptItem::External(u) = item {
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

            // Now assemble sources in document order: inline goes through
            // unchanged, externals come from the parallel-fetch results map
            // (skipped if their fetch failed).
            let mut sources: Vec<String> = Vec::new();
            for (idx, item) in items.into_iter().enumerate() {
                match item {
                    ScriptItem::Inline(s) => {
                        inline_count += 1;
                        sources.push(s);
                    }
                    ScriptItem::External(_) => {
                        if let Some(body) = external_results.remove(&idx) {
                            sources.push(body);
                        }
                    }
                }
            }
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
            for source in &sources {
                if let Err(e) = self.eval_void(source) {
                    let msg = e.to_string();
                    let is_interrupt = msg.contains("interrupted");
                    if is_interrupt {
                        interrupted += 1;
                    }
                    if msg.len() > 200 {
                        eval_errors.push(format!("{}…", &msg[..200]));
                    } else {
                        eval_errors.push(msg);
                    }
                } else {
                    executed += 1;
                }
            }

            // Restore the dispatcher's outer deadline so settle's pumps run
            // under the broader navigate budget rather than the tight 5s
            // script-phase one. (Settle pump callbacks are bounded too — they
            // run inside QuickJS evals which still consult the same atomic.)
            self.restore_eval_deadline(prev_deadline);
            // Fire DOMContentLoaded → settle → load → settle.
            let _ = self
                .eval("typeof __fireDOMContentLoaded === 'function' && __fireDOMContentLoaded()");
            let after_dcl = self.settle(2000, 100).await.ok();
            let _ = self.eval("typeof __fireLoad === 'function' && __fireLoad()");
            let after_load = self.settle(1500, 50).await.ok();
            Some(json!({
                "inline_count": inline_count,
                "external_count": external_count,
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

        Ok(json!({
            "status": status,
            "url": final_url,
            "bytes": bytes,
            "headers": Value::Object(headers),
            "blockmap": blockmap,
            "challenge": challenge,
            "scripts": scripts,
            "extract": auto_extract,
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

        loop {
            if iters >= max_iters {
                break;
            }
            let elapsed_ms = start.elapsed().as_millis() as u64;
            if elapsed_ms >= max_ms {
                break;
            }

            // 1. Drain microtasks.
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
                if mt_this_iter > 10_000 {
                    break; // safety against infinite microtask loops
                }
            }
            total_microtasks += mt_this_iter;

            // 2. Pump expired timers.
            let fired = self.eval("__pumpTimers()")?.as_u64().unwrap_or(0);
            total_timers += fired;

            // 3. Drain fetch responses (resolves pending Promises JS-side).
            let resolved = self.eval("__pollFetches()")?.as_u64().unwrap_or(0);
            total_fetches += resolved;

            // 4. Decide whether to keep going.
            let pending_timers = self.eval("__pendingTimers()")?.as_u64().unwrap_or(0);
            let pending_fetches = self.eval("__pendingFetches()")?.as_u64().unwrap_or(0);
            let microtasks_pending = self.js_rt.is_job_pending();

            if pending_timers == 0 && pending_fetches == 0 && !microtasks_pending {
                break; // queue fully empty
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
            "timed_out": iters >= max_iters || elapsed_ms >= max_ms,
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

// One <script> element from a parsed page — either an inline body or an
// external src= URL (resolved against the page URL).
enum ScriptItem {
    Inline(String),
    External(String),
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
                    out.push(ScriptItem::External(resolved.to_string()));
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
    if args.iter().any(|a| a == "--list-profiles") {
        for n in profile::Profile::list_builtin() {
            println!("{n}");
        }
        return Ok(());
    }
    let profile_name = parse_profile_arg(&args);
    let profile = Profile::load(&profile_name)?;
    if args.iter().any(|a| a == "--mcp") {
        mcp_main(profile).await
    } else {
        rpc_main(profile).await
    }
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
    let profile_name = profile.name.clone();
    let mut session = Session::new(&profile)?;
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
            "description": "Fetch a URL with Chrome-fingerprinted HTTP (rquest, Chrome 131 emulation). Parses HTML, seeds the JS DOM, returns BlockMap inline. With `exec_scripts: true`, also extracts inline <script> tags from the parsed HTML, eval's them in QuickJS (with shims for setTimeout/fetch/etc.), then settles the event loop and fires DOMContentLoaded + load. Returns a `scripts` summary with executed/errors when exec_scripts is true.\n\nAuto-extract: when the page embeds JSON-bearing <script> tags (density.json_scripts > 0 — covers application/json, application/ld+json, text/x-magento-init, text/x-shopify-app, etc.), navigate auto-runs `extract()` and returns the result as the `extract` field. Saves a round trip on the common case where the data the JS would have rendered is already sitting in the HTML — JSON-LD article schemas on news sites, __NEXT_DATA__ page state on Next.js apps, json_in_script product blobs on Magento/Shopify, GitHub RSC payloads, etc. Pages with no embedded JSON (density.json_scripts == 0) get extract:null and pay zero extra cost. The agent can still call extract() explicitly to force a specific strategy.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url":          { "type": "string", "description": "Absolute URL to fetch" },
                    "exec_scripts": { "type": "boolean", "description": "Execute inline <script> tags after parse (default false). External src=URL scripts are NOT loaded yet." }
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
        _ => Err(anyhow!("unknown tool: {name}")),
    }
}

async fn mcp_main(profile: Profile) -> Result<()> {
    let mut session = Session::new(&profile)?;
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
