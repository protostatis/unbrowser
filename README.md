# unbrowser

**Web access for LLM agents. One static binary. No Chrome.**

### Install

**Python (recommended)** — wheel ships the native binary. **Requires Python 3.10+**:

```bash
pipx install pyunbrowser   # cleanest on macOS Homebrew / modern Linux (handles PEP 668)
pip  install pyunbrowser   # in a venv on python3.10+
```

> **macOS gotcha**: the system `/usr/bin/python3` is 3.9 and the wheel will reject it with "requires Python >=3.10". Use Homebrew's `python3.13` or `pipx` (which manages its own Python). If `pip install` fails with PEP 668 ("externally-managed-environment"), that's the same issue — `pipx install pyunbrowser` is the right call.

```python
from unbrowser import Client       # note: pip name is pyunbrowser, import is unbrowser
with Client() as ub:                # (PyPI's name moderation blocks 'unbrowser';
    r = ub.navigate("https://news.ycombinator.com")   # py- prefix is the standard workaround)
```

**Cargo** — binary only, no Python wrapper:

```bash
cargo install unbrowser
unbrowser --mcp
```

**Pre-built tarball** — for systems without Python or Rust:

```bash
# macOS Apple Silicon
curl -L https://github.com/protostatis/unbrowser/releases/latest/download/unbrowser-aarch64-apple-darwin.tar.gz | tar xz
# Linux x86_64 (glibc 2.35+)
curl -L https://github.com/protostatis/unbrowser/releases/latest/download/unbrowser-x86_64-unknown-linux-gnu.tar.gz | tar xz
```

**macOS Intel**: no prebuilt — `cargo install unbrowser` is the path. (GitHub's `macos-13` runner pool is being deprecated and CI allocations were exceeding an hour. We'll re-enable Intel Mac wheels via cross-compile from Apple Silicon in a future release.)

**From source**:

```bash
cargo build --release   # binary at ./target/release/unbrowser
```

### Bare RPC

```bash
echo '{"id":1,"method":"navigate","params":{"url":"https://news.ycombinator.com"}}' | unbrowser
```

That's the install. Runs anywhere a static binary runs — laptop, Lambda, Cloudflare Workers, edge, embedded.

Open source under Apache 2.0. Hosted at **[unchainedsky.com](https://unchainedsky.com)** if you'd rather skip the ops.

---

## By the numbers

|                | This binary    | Headless Chrome (Playwright/Puppeteer) |
|----------------|----------------|-----------------------------------------|
| Binary size    | **~10MB**      | 250MB+ Chrome download                  |
| RAM / session  | **~50MB**      | 200–500MB                                |
| Cold start     | **~100ms**     | ~1s                                      |
| Tokens / page (LLM) | **~500** (BlockMap inline) | tens of thousands of HTML, parsed by you |
| Install steps  | `cargo build`  | install Chrome + Node + Playwright + system deps |
| Lambda / Workers / edge | ✅      | ❌ Chrome too big                        |
| 100K pages/day cost | $0 (your infra) | $$$ Chrome fleet or hosted API     |

**5–10× lower memory, 25× smaller binary, 10× faster cold start, 70× lower per-page token cost.** That's the tradeoff this product makes — defer JS-rendering (Phase 4/5) and pixel rendering (out of scope) in exchange for a footprint that fits in places Chrome doesn't.

## Agent-friendly by design

This isn't a Chrome wrapper that an agent uses through a Puppeteer-shaped abstraction. It's a browser whose every output is shaped for LLM consumption:

- **`navigate` returns a BlockMap** — ~500 tokens of structured page summary (landmarks, headings, interactives, density signals) right in the response. No follow-up call needed to know what's on the page.
- **Stable element refs** (`e:142`) — query, click, type, submit using opaque handles. The LLM never has to scrape the DOM itself.
- **`challenge` field on every blocked navigate** — provider, confidence, and the exact clearance cookie name. The agent reacts intelligently instead of guessing.
- **`density.likely_js_filled` heuristic** — distinguishes "real SSR page" from "SSR shell with JS-filled cells" (the CNBC trap). The agent bails before burning round-trips on a page it can't read.
- **MCP-native** — `unbrowser --mcp` exposes 12 tools to any MCP host (Claude Code, Claude Desktop, Cursor, Cline). 4 lines of config, zero glue code.
- **Real Chrome fingerprint** (Chrome 131 JA4 + Akamai H2 hash) so sites don't block you for being a script.

For pages that *do* need real Chrome (heavy SPAs, JS-challenge bot walls), the binary detects them and accepts cookies via `cookies_set` — so you solve once in Chrome and replay forever here.

## Quick demo — Hacker News top 3

```python
from unbrowser import Client

with Client() as ub:
    ub.navigate("https://news.ycombinator.com")
    for s in ub.query(".titleline > a")[:3]:
        print(s["text"], s["attrs"]["href"])
```

5 lines, no headless browser install. Output is structured JSON, not 35KB of HTML. The `Client` wrapper handles subprocess lifecycle (atexit reaper so orphans are impossible), JSON-RPC framing, and surfaces real exceptions instead of silent `result` lookups.

<details>
<summary>Bare-RPC version (if you can't use Python)</summary>

The same demo without the wrapper — useful for languages other than Python or one-shot bash calls. The protocol is JSON-RPC over stdin/stdout, one JSON object per line:

```python
import subprocess, json
p = subprocess.Popen(["./target/release/unbrowser"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
i = 0
def call(method, **params):
    global i; i += 1
    p.stdin.write(json.dumps({"id": i, "method": method, "params": params}) + "\n")
    p.stdin.flush()
    return json.loads(p.stdout.readline())["result"]

call("navigate", url="https://news.ycombinator.com")
for s in call("query", selector=".titleline > a")[:3]:
    print(s["text"], s["attrs"]["href"])
```

That's the entire protocol surface. Same shape from any language with subprocess + JSON.

</details>

## SPA tier — what works, what doesn't

Empirical, not aspirational. Latest matrix: **28/30** on tested categories.

| Page tier | Coverage | What to expect |
|---|---|---|
| **Static + SSR** (Wikipedia, MDN, news, docs, GitHub repo browsing, search engines, archive.org) | ✅ excellent | sub-second navigate; full BlockMap; all selectors work; ~hundreds of tokens vs ~tens of KB raw |
| **SSR + light hydration** (Next.js docs, marketing pages, react.dev's *static* content) | ✅ usable | reads SSR'd content fine; hydration adds nothing but doesn't break either |
| **Bot-walled with cookie handoff** (Zillow, Cloudflare-protected sites) | ✅ via `cookies_set` | solve once in Chrome, replay forever; `challenge.provider` field tells the agent which vendor |
| **Module-loader SPAs** (Ember, AMD apps like crates.io) | ⚠️ partial with `exec_scripts: true` | bundles fetch + execute, modules register, but framework auto-mount needs case-by-case shimming |
| **Heavy React/Vue bundles** (react.dev runtime, large dashboard apps) | ⚠️ bounded — won't hang, won't render | with `exec_scripts: true` the navigate completes inside the 30s wall-clock budget (5s for the script-eval phase, the rest for settle); rendered DOM may not materialize. Tune via `UNBROWSER_TIMEOUT_MS` |
| **Apps requiring Workers / Canvas / IndexedDB / WebGL** | ❌ out of scope by design | use the cookie-handoff path with real Chrome via [unchainedsky.com](https://unchainedsky.com) |
| **Hardest-tier anti-bot** (PerimeterX with behavioral, Kasada, Akamai BMP advanced) | ❌ even cookie handoff is fragile | managed service is the right tier |

**Vs the alternatives:**

| | This | curl | Playwright / headless Chrome |
|---|---|---|---|
| Static / SSR pages | ✅ | ✅ but token-heavy | overkill |
| SPA-shell sites | ⚠️ partial via `exec_scripts` | ❌ | ✅ |
| Bot-walled (with cookie handoff) | ✅ | ❌ | ✅ |
| Run in Lambda / Workers / edge | ✅ | ✅ | ❌ Chrome too big |
| Per-page cost at 100K/day | ~free | ~free | $$$ |
| LLM-shaped output | ✅ BlockMap inline | DIY parse | DIY parse |

## Verified against (working)

Concrete sites tested with measured times. Cold-start to extracted-result.

| Category | Sites | Time |
|---|---|---|
| Reference / docs | Wikipedia, MDN, docs.rs, PyPI, react.dev (SSR portion) | 0.9 – 5.8s |
| News | Hacker News, BBC, TechCrunch, ArXiv listings | 1 – 1.6s |
| Search | Google `/search`, Bing, Brave, DuckDuckGo (html) | 0.2 – 1.8s |
| Dev | GitHub repo pages, npm, StackOverflow, HuggingFace model cards | 0.7 – 2.4s |
| Crypto / finance | CoinGecko, Yahoo Finance (post-redirect-fix) | 3.5 – 6.9s |
| Social | Lobsters, old.reddit.com | 0.9 – 1.4s |
| Govt / institutional | arXiv, archive.org, gov.uk | 0.6 – 1.0s |
| Interaction primitives | type, click + auto-follow, cookies_set/get/replay, eval, query_text | 0.3 – 1.3s |

**Surprises:** all four major search engines work cleanly. CoinGecko's heavy dashboard SSRs enough that quotes come through. HuggingFace model cards expose model name in `<h1>`.

## Bot-detection diagnostics

Every blocked navigate returns a `challenge` field naming the vendor (`perimeterx_block`, `cloudflare_turnstile`, `aws_waf`, `datadome`, `akamai_bmp`, `imperva`, `arkose_labs`, `recaptcha`, `press_hold`, `yahoo_sad_panda`, `interstitial`, `generic_human_verification`, `unknown_block`) plus the expected clearance cookie name. Agents react with cookie handoff via `cookies_set` instead of guessing.

## SPA-detection diagnostics

Every navigate's `blockmap.density` field signals SPA-ness so agents bail before wasting round-trips:
- `thin_shell: true` — page is < 4KB body text with no headings or interactives (typical React/Ember root)
- `likely_js_filled: true` — `<table>` shells exist but are empty (CNBC-class trap)
- `json_scripts: N` — count of `<script type="application/json">` (often holds the data the JS would render — try `eval()` on those before escalating)

## Three ways agents talk to it

### MCP (no glue)

```json
{"mcpServers":{"unchained":{"command":"unbrowser","args":["--mcp"]}}}
```

12 tools auto-discovered by Claude Code, Claude Desktop, Cursor, Cline.

### Subprocess (custom runtimes)

13 lines of Python (above). Or any language with subprocess + JSON.

### Auto-escalation router (`scripts/router.py`)

```python
from scripts.router import Router, RouterConfig, cached_cookies_solver

with Router(RouterConfig(
    binary="./target/release/unbrowser",
    chrome_solver=cached_cookies_solver("cookies.json"),
)) as r:
    r.navigate("https://www.zillow.com/homes/for_rent/")  # auto-handles 403 + cookie replay
```

### Live event watcher (`scripts/watch.py`)

The binary emits NDJSON events (`ready`, `navigate`, `challenge`) on stderr. Pipe them through `watch.py` for color-coded one-liners:

```bash
unbrowser 2> >(python3 scripts/watch.py)
```

## RPC methods

| | |
|---|---|
| `navigate {url}` | fetch + parse + return `{status, url, bytes, headers, blockmap, challenge}` |
| `query {selector}` | CSS query → `[{ref, tag, attrs, text}]` |
| `text {selector?}` | textContent of FIRST match (default `body`). On Wikipedia/MDN/news sites the first `<p>` is often a hatnote — prefer `text_main` for article body. |
| `text_main` | textContent of `<main>` / `[role=main]` / single `<article>` / longest non-chrome subtree. Use this for reading article/docs/blog content. |
| `click {ref}` | dispatch click; auto-follows `<a href>` (returns `{status, url, bytes, headers, blockmap, challenge}` — same shape as `navigate`) |
| `type {ref, text}` | set value + dispatch input/change events |
| `submit {ref}` | gather GET-form fields + navigate |
| `eval {code}` | run JS in embedded QuickJS |
| `cookies_set / cookies_get / cookies_clear` | session jar |
| `blockmap` | recompute the page summary |
| `body` | raw HTML of last navigation |

CSS selector engine: tag, id, class, `[attr=val]` (also `^=`, `$=`, `*=`, `~=`), all four combinators (` `, `>`, `+`, `~`), `:first/last/nth-child/of-type`, `:only-child/of-type`. Use `eval` for `:not()`, `:has()`, formulas.

## Self-host vs managed

| | This binary | [unchainedsky.com](https://unchainedsky.com) |
|---|---|---|
| Cheap-path scraping | ✅ | ✅ |
| Real-Chrome escalation | DIY | ✅ included |
| Cookie cache across workers | DIY | ✅ |
| Built-in Claude agent loop | DIY | ✅ |
| Time to first scrape | one cargo build | one API key |
| You own the ops | ✅ | nope, we do |

The vocabulary is the same. Code written against this binary works against the hosted service.

## Honest limits

- **Script execution is opt-in via `exec_scripts: true`.** Default navigate skips it (the SSR/static path is what most agents want). With it on, inline + external `<script>` tags run in QuickJS — works for many SPAs, but heavy framework bootstraps (Ember, big React) often don't auto-mount because shims can't fake every browser-specific signal. The blockmap's `density.likely_js_filled` flag tells agents in one call when to escalate instead of burning round-trips.
- **All eval is wall-clock bounded.** A 30s watchdog (configurable via `UNBROWSER_TIMEOUT_MS`, clamped to 1s..10min) covers script execution AND every subsequent settle/microtask/timer callback, so a hostile site can never wedge the binary or strand a CPU-pegged orphan process.
- **GET-only form submit.** POST/multipart errors out — construct the request manually via `eval` or escalate.
- **Hardest-tier bot detection** (PerimeterX with behavioral telemetry, advanced Akamai BMP, Kasada) needs the cookie-handoff path. The binary detects and labels the challenge for you, but solving it requires real Chrome (or a token vendor).
- **No screenshots.** Out of scope by design.

## Build

Rust 1.95+ via [rustup](https://rustup.rs). On macOS, also `brew install cmake ninja` (BoringSSL dependency).

```bash
cargo build --release
```

~2 min first build (BoringSSL compiles), instant after.

## Architecture in one diagram

```
JSON-RPC stdin ─┐    ┌─ stdout
                ▼    ▲
         ┌────────────────────┐
         │  rquest (Chrome131 │   ┌──────────┐    ┌──────────────────┐
         │  TLS+H2 fingerprint)├──▶ html5ever ├───▶ rquickjs +       │
         │                    │   │  parser  │    │  dom.js +        │
         │  cookie_store      │   └──────────┘    │  blockmap.js +   │
         │  (jar)             │                   │  interact.js     │
         └────────────────────┘                   └──────────────────┘
```

## License

Apache 2.0 — see [LICENSE](./LICENSE).

---

If this is useful and you'd rather not run it yourself: **[unchainedsky.com](https://unchainedsky.com)** is the hosted version, same vocabulary, no ops.
