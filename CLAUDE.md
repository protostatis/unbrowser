# unbrowser

Single statically-linked native binary that gives an LLM a headless browser with real JS execution. No rendering — designed for LLM tool-call use, not human viewing.

Distinct from `~/Projects/sky-search` (QuickJS-WASM in user's browser, with iframe rendering for humans). This project ports several JS modules from sky-search but drops rendering, the WebSocket/server layers, and the iframe bridge entirely.

## Why this exists

The four-way landscape:

| | curl/fetch | LLM WebFetch/Search | unbrowser | Full Chrome |
|---|---|---|---|---|
| Runs JS | No | No (provider-side, opaque) | Yes (QuickJS) | Yes (V8, JIT) |
| SPA-capable | No | Sometimes | Yes | Yes |
| Interactive (click/type/auth) | No | No | Yes | Yes |
| Session across calls | DIY | No | Yes | Yes |
| Output to LLM | Raw HTML (heavy) | Pre-summarized markdown | BlockMap (semantic, low-token) | Raw DOM or screenshot (very heavy) |
| Memory | ~0 | N/A | tens of MB | 200–500MB+ per page |
| Deploys as | one syscall | API call | one binary | a fleet |

**The gap this fills:** stateful, JS-required, non-visual web automation for LLM agents — log in, fill forms, navigate SPAs, scrape dashboards, follow multi-step flows — at one-binary deploy cost, with output already shaped for token efficiency.

**Positioning:** not "Chrome but smaller" (Chrome is *the* compatibility target — that framing loses). It's **"WebFetch but stateful and interactive"**, or **"curl but the page actually runs"**. The LLM-native output (BlockMap + element refs) is the differentiator that neither curl nor Chrome give you for free.

**Honest weak spots vs Chrome:** heavy JIT-bound JS (QuickJS is 20–50× slower); anti-bot that fingerprints canvas/WebGL/audio; anything visual (screenshot agents, captcha OCR); sites that lazy-load via real intersection observer + viewport metrics.

## Architecture (locked)

- **Native, not WASM.** Rust host. Crates: `rquickjs`, `html5ever`, **`rquest`** (Chrome-fingerprinted HTTP client — see Stealth), `tokio`, `url`, `serde_json`.
- **Runs page scripts** so the DOM settles. SPA-capable.
- **Interactive from day one** — clicks, typing, event synthesis.
- **No rendering.** Replacement for human monitoring is a **BlockMap**: DOM walk → semantic block tree → DDM-style ASCII grid + structured JSON. Same source produces both LLM and human views.
- **CSS selectors stay JS-side for v1** (reuse sky-search `dom.js`).
- **Protocol:** JSON-RPC over stdin/stdout for commands; NDJSON event stream on stderr for observability.
- **Element refs** (`e:142`) returned by query commands are stable handles into the VDOM.
- **Stealth is a first-class concern, not a flag.** See below.

## Stealth mode (native, on by default)

Bot detection is the #1 reason this product gets blocked, so stealth is wired through every layer rather than retrofitted. Driven by versioned **profiles** (`profiles/chrome_134.toml` etc.) so all the signals stay coherent and a Chrome bump is one config change.

**The six layers:**

1. **TLS fingerprint (JA3/JA4)** — `rquest`, not `reqwest`. Mimics Chrome's BoringSSL cipher order, extensions, ALPN. Set at connection time, can't be added later.
2. **HTTP/2 frame ordering** — Akamai's H2 hash. Also handled by `rquest`.
3. **Headers** — coherent UA, `Sec-CH-UA` / `Sec-CH-UA-Mobile` / `Sec-CH-UA-Platform`, `Accept-Language`, `Accept-Encoding` (br/gzip order matters), header *ordering*. Profile-driven.
4. **JS environment** (in `shims.js`) — `navigator.webdriver = undefined`, populated `navigator.plugins` / `navigator.languages` / `navigator.platform`, faked `window.chrome`, `Notification.permission` quirks, realistic `screen` dims, timezone, locale.
5. **Canvas / WebGL / AudioContext** — we don't render, so these are forged. Stable, plausible responses to `canvas.toDataURL`, `getImageData`, `WebGL getParameter`, `AudioContext.createOscillator`, etc. Per-profile seed so the fingerprint is consistent across a session but distinct across instances.
6. **Behavioral** — realistic event timing in the interactivity layer (click ≠ instant, typing has inter-key jitter, scroll has easing).

**Honest tier-of-detection ceiling:**

- ✅ Defeats: Cloudflare Bot Fight Mode (commodity tier), Datadome (basic), Akamai BMP (light), PerimeterX (light), most homegrown header/UA checks
- ❌ Won't beat: FingerprintJS Pro at high sensitivity, Cloudflare Turnstile interactive challenges, Kasada, Arkose Labs MatchKey. Those need real Chrome + residential IP.

**Maintenance:** Chrome ships every ~4 weeks; UA + Sec-CH-UA + cipher list drift constantly. Profile bumps must be cheap — single TOML edit, regenerate, test against a small fingerprinting site corpus.

## Host functions exposed to JS

About seven total:

- `__host_fetch` — HTTP via `rquest` with active stealth profile
- `__host_parse_html` — html5ever, seeds VDOM
- `__host_set_timer` / `__host_clear_timer`
- `__host_emit` — NDJSON event to stderr
- `__host_now`
- `__host_fp` — deterministic per-session fingerprint values for canvas/WebGL/audio forging

## Reuse from sky-search

Port as embedded JS strings via `include_str!`:

- `/Users/zhiminzou/Projects/sky-search/client/wasm/dom.js` (731 LOC)
- `/Users/zhiminzou/Projects/sky-search/client/wasm/shims.js` (937 LOC) — fetch/XHR/timers/URL/localStorage stub. **Will need stealth additions** (canvas/WebGL/audio forging, navigator patches) — sky-search doesn't have these because the parent browser provides them.
- `/Users/zhiminzou/Projects/sky-search/client/wasm/intel.js` (161 LOC)

Skip `bridge.js` (no iframe). Skip the WebSocket/server layers entirely.

## Phased build plan

1. ✅ **Skeleton** — Cargo, rquickjs, JSON-RPC stdin/stdout loop, `eval` + `close` commands
2. ✅ **Fetch + parse** — `rquest` navigate with Chrome profile, html5ever parse, DOM seeded into JS. TLS+H2 fingerprint verified Chrome 131 against `tls.peet.ws`.
3. ✅ **Virtual DOM** — `dom.js` ported (un-wrapped from sky-search template literal into `src/js/dom.js`), seeded from html5ever tree, `query` / `text` RPC commands working.
4. **Shims + stealth JS layer** — port `shims.js`; add navigator/window.chrome patches, canvas/WebGL/audio forging via `__host_fp`
5. ✅ **Page execution** — `navigate {exec_scripts: true}` extracts inline + external `<script>` tags via `walk_for_scripts` in `main.rs`, parallel-fetches externals (8s per-fetch timeout) using `self.http`, then evals all in document order in QuickJS under a 5s script-phase budget. `<script async>` honored: async scripts execute after the sync queue (see `ScriptKind` enum). Settle loop alternates microtasks → JS-side `__pumpTimers` → `__pollFetches` until idle, max 100 iters / 2s. Fires `__fireDOMContentLoaded` then settles, then `__fireLoad` then settles. Watchdog interrupt handler aborts runaway scripts at the dispatch deadline. Returns `scripts: {inline_count, external_count, async_count, policy_blocked, executed, interrupted, errors, fetch_errors, settle_after_dcl, settle_after_load}`. End-to-end verified on HN/CNBC/Forbes/The Verge — see `scripts/policy_e2e.json`.
5b. ✅ **Policy hook in script-fetch path** — when `--policy=blocklist` is set, `policy::decide(url)` gates each external `<script src>` BEFORE its fetch task is spawned. Tracker URLs (Adobe DTM, Ketch, GoogleTagServices, DoubleVerify, Amazon adsystem, etc.) emit `policy_blocked` NDJSON events and are skipped. Blocked count surfaces in the navigate result's `scripts.policy_blocked`. The Verge: 7/25 static script tags blocked. Anti-bot/fingerprinting hosts (FingerprintJS, PerimeterX, Datadome, Cloudflare Turnstile, hCaptcha, reCAPTCHA, Arkose, Imperva) intentionally NOT blocked — locked in by `stealth_safety_no_fingerprinting_hosts` test in `src/policy.rs`.
6. ✅ **BlockMap** — `src/js/blockmap.js` walks DOM landmarks/headings/interactives, returns structured JSON + ASCII outline. **`navigate` returns blockmap inline** so the agent gets one-shot orientation. Verified: HN (no semantic tags) → fallback to significant top-level children; Wikipedia (full landmarks) → header/nav/main/footer with refs.
7. ✅ **Interactivity (v1)** — `src/js/interact.js` provides `__byRef`, `__click`, `__type`, `__formData`. RPC methods `click` / `type` / `submit`. Click on `<a href>` auto-follows (resolves relative URLs against current page, then navigates). Submit is GET-only for v1. Verified end-to-end: HN → `type rquest` into `input[name=q]` → `submit` → landed on `https://hn.algolia.com/?q=rquest`.
8. (skipped 8/9/10 numbering)
11a. ✅ **Cookie jar** — `CookieJar` struct in `main.rs` implements `rquest::cookie::CookieStore`. Cookies in response `Set-Cookie` headers auto-populate; cookies are auto-sent on subsequent requests to matching domains. RPC methods `cookies_set` (object or string form, accepts `{name, value, domain, path?, secure?, http_only?, url?}`), `cookies_get`, `cookies_clear`. Persistence (file save/load) is the agent driver's responsibility — the binary is stateless. **The killer use case:** solve a PerimeterX/Datadome/Cloudflare challenge once in real Chrome, copy the clearance cookie via DevTools, paste into a `cookies_set` call, run unbrowser against the protected site for the cookie's lifetime. End-to-end verified against `zillow.com/homes/for_rent/`: 403+captcha without cookies → 200+626KB rentals page with cookies replayed.
12. ✅ **MCP server mode** — `unbrowser --mcp` enters Model Context Protocol mode: JSON-RPC 2.0 on stdio with `initialize` / `tools/list` / `tools/call` / `ping`. All 12 RPC methods exposed as MCP tools (everything except `close` — host manages lifecycle). JSON schemas inline for tool discovery. Result content returned as pretty-printed JSON in a single `text` content item. Errors set `isError: true`. Verified via Python driver: handshake → tools/list → tools/call navigate → tools/call query with sibling combinator → bad-tool-name returns isError=true.
13. ✅ **Challenge detector aligned with private-core** — `detect_challenge` in main.rs now uses private-core's vendor names (`perimeterx_block`, `cloudflare_turnstile`, `arkose_labs`, `recaptcha`, `press_hold`, `generic_human_verification`) plus `datadome`, `akamai_bmp`, `imperva` from prior version. Output shape `{blocked, provider, confidence, status, matched, clearance_cookie, reason, hint}` matches private-core's `ChallengeDetectionResult` plus actionability fields (`clearance_cookie`, `hint`). Picks highest-confidence match (not first), so e.g. an `arkose_labs` page mentioning "captcha" reports as arkose, not generic.
14. ✅ **Auto-escalation router** — `scripts/router.py`. Wraps the binary, intercepts `navigate`, inspects `challenge` field, calls a pluggable `chrome_solver(url) -> [cookies]` callback when blocked, replays via `cookies_set`, retries. Reference solvers: `cached_cookies_solver(path)` (loads from JSON file — supports CDP and ub formats), `unchained_cli_solver(profile)` (shells out to existing unchainedsky CLI). Errors clearly when blocked with no solver. End-to-end verified: HN clean (no escalation), Zillow no-solver (helpful error), Zillow cached-solver (replays 51 cookies, gets real listings).
8. **Intel** — port `intel.js`, `extract` command with auto-strategy
9. **Profile system** — `profiles/chrome_*.toml`, profile selector, fingerprint test harness against a known FP-detection corpus
10. **TUI viewer** — separate `unbrowser watch` binary tailing NDJSON

## Current RPC methods

- `eval {code}` — run arbitrary JS in the session, returns JSON-stringified result. JS exceptions surface with their actual `.name` and `.message` (e.g. `TypeError: cannot read property 'foo' of null`) — the binary calls `ctx.catch()` to extract the pending exception rather than reporting the generic "Exception generated by QuickJS".
- `navigate {url}` — `rquest` GET with Chrome131 emulation; parses HTML with html5ever; seeds the JS DOM. Returns `{status, url, bytes, blockmap, challenge}`. The blockmap is the one-shot orientation payload. The `challenge` field is `null` on the happy path; on bot-detection responses it's `{vendor, status, clearance_cookie, hint}` so the agent can react (e.g. escalate to real Chrome). Detected vendors: PerimeterX, Cloudflare, Datadome, Akamai BMP, Imperva.
- `body` — raw HTML of the last navigation (debugging / fallback).
- `query {selector}` — `document.querySelectorAll`, returns `[{ref, tag, attrs, text}]`. Element refs (`e:NN`) are stable handles into the VDOM. Selector engine in `dom.js` supports: tag, id, class, attribute (`=`, `^=`, `$=`, `*=`, `~=`), all four combinators (` `, `>`, `+`, `~` — with or without surrounding spaces), and pseudo-classes `:first-child`, `:last-child`, `:first-of-type`, `:last-of-type`, `:nth-child(N|odd|even)`, `:nth-of-type(N|odd|even)`, `:only-child`, `:only-of-type`. **Not yet supported:** `:not()`, `:has()`, `An+B` formulas inside `nth-*`. The smart tokenizer respects `[]` and `()` depth so attribute selectors with `~` and pseudo-class args with `+` aren't broken up.
- `text {selector?}` — `textContent` of first match (default `body`).
- `blockmap` — recompute the BlockMap (use after eval'd JS mutates the DOM). Same shape as the inline blockmap from `navigate`.
- `click {ref}` — dispatch a `click` event on the element at `ref` (e.g. `e:142`). If the element is `<a href>` and the click wasn't `preventDefault`'d, auto-follows the href via `navigate` (returns the navigation result). Otherwise returns `{ok, ref, tag, follow: null}`.
- `type {ref, text}` — set `el.value` + `el.setAttribute('value', ...)`, then dispatch `input` and `change` events. Currently no inter-key timing jitter (TODO for v2).
- `submit {ref}` — gather GET-form field values, build query string, navigate to action URL. v1 supports GET only; POST/multipart errors out. Checkboxes/radios skipped (we don't track checked state yet).
- `cookies_set {cookies: [...], url?}` — add cookies to the jar. Each item can be a Set-Cookie string or `{name, value, domain?, path?, secure?, http_only?, url?}`. Returns `{added: N}`.
- `cookies_get` — list current cookies as `[{name, value, domain, path, secure, http_only}, ...]`. The agent driver handles file save/load.
- `cookies_clear` — drop all cookies.
- `close` — exit cleanly.

## MCP integration

Add to your MCP host's config (Claude Desktop's `claude_desktop_config.json`, Claude Code's `.mcp.json`, etc.). Once `unbrowser` is on `$PATH` (via `pip install pyunbrowser`, `cargo install unbrowser`, or `brew install unbrowser`):

```json
{
  "mcpServers": {
    "unbrowser": {
      "command": "unbrowser",
      "args": ["--mcp"]
    }
  }
}
```

For a development checkout, use the explicit path: `command: "/path/to/repo/target/release/unbrowser"`. All RPC methods are exposed as MCP tools (everything except `close` — host manages lifecycle); discoverable via `tools/list`.

**BlockMap shape:** `{title, structure: [{role, ref, ident, counts, summary}], headings: [{level, text, ref}], interactives: {links, buttons, inputs: [...], forms: [...]}, density: {tables, td, li, json_scripts, likely_js_filled}, ascii: "<multiline>"}`. The `ascii` field is human-readable; `structure`/`headings`/`interactives`/`density` are what the agent uses to plan queries. Landmarks (`header, nav, main, aside, footer, article, section`) are detected first; if none, the walker falls back to significant top-level children of `<body>`.

**`density` field** — distinguishes "fully SSR'd" from "SSR shell with JS-filled cells" (the CNBC trap). Each of `tables`/`td`/`li` is `{total, filled, ratio}` or null. `likely_js_filled: true` means agents should NOT commit to a long extraction — try eval'ing `script[type=application/json]` first (the `json_scripts` count tells you if any exist), or escalate to real Chrome. Verified discrimination: HN/Wikipedia/Yahoo Finance → false; CNBC (6 empty `<table>`s) → true.

## Notes

- QuickJS perf is ~20–50× slower than V8 on JIT-heavy code. Flag if heavy SPAs come up.
- `rquest` is the load-bearing crate for stealth. If it goes unmaintained, fall back to `boring` (BoringSSL) + custom `hyper` configuration.
- `html5ever` and `markup5ever_rcdom` versions must align (they share the `markup5ever` data model). Currently pinned to `=0.38.0` for both — html5ever 0.39 has no matching `markup5ever_rcdom` release yet.
- Build deps: `cmake` + `ninja` (for BoringSSL via `boring-sys2`). Install via brew on macOS. Rust 1.95+ via rustup.
