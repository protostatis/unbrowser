# unchained_browser

**Web access for LLM agents. One static binary. No Chrome.**

```bash
cargo build --release
echo '{"id":1,"method":"navigate","params":{"url":"https://news.ycombinator.com"}}' \
  | ./target/release/unchained_browser
```

That's the install. ~10MB binary, ~50MB RAM/session, ~100ms cold start. Runs anywhere a static binary runs — laptop, Lambda, Cloudflare Workers, edge, embedded.

Open source under Apache 2.0. Hosted at **[unchainedsky.com](https://unchainedsky.com)** if you'd rather skip the ops.

---

## Why

Most agent web tasks don't need Chrome. They need:

- A real Chrome TLS fingerprint (so sites don't block you)
- A parsed DOM with CSS selectors
- Cookies that persist across calls
- A small, structured page summary the LLM can reason about

That's what this is. ~700 lines of Rust + ~1000 lines of embedded JS. No Playwright dependency, no headless Chrome download, no Selenium grid.

For pages that *do* need real Chrome (heavy SPAs, JS-challenge bot walls), the binary surfaces a `challenge` field on every blocked navigate and accepts cookies via `cookies_set` — so you can solve once in Chrome, replay forever here.

## Quick demo — Hacker News top 3

```python
import subprocess, json
p = subprocess.Popen(["./target/release/unchained_browser"],
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

13 lines, no dependencies, no headless browser install. The output is structured JSON, not 35KB of HTML.

## When to use

| | This | curl | Playwright / headless Chrome |
|---|---|---|---|
| Static / SSR pages | ✅ | ✅ but token-heavy | overkill |
| Bot-walled (with cookie handoff) | ✅ | ❌ | ✅ |
| SPA whose data is JS-rendered | ❌ until Phase 4/5 | ❌ | ✅ |
| Run in Lambda / Workers / edge | ✅ | ✅ | ❌ Chrome too big |
| Per-page cost at 100K/day | ~free | ~free | $$$ |
| LLM-shaped output | ✅ BlockMap inline | DIY parse | DIY parse |

If most of your pages are static or SSR, this. If most are SPAs, Playwright. If you don't want to run anything, see [unchainedsky.com](https://unchainedsky.com).

## Three ways agents talk to it

### MCP (no glue)

```json
{"mcpServers":{"unchained":{"command":"unchained_browser","args":["--mcp"]}}}
```

12 tools auto-discovered by Claude Code, Claude Desktop, Cursor, Cline.

### Subprocess (custom runtimes)

13 lines of Python (above). Or any language with subprocess + JSON.

### Auto-escalation router (`scripts/router.py`)

```python
from scripts.router import Router, RouterConfig, cached_cookies_solver

with Router(RouterConfig(
    binary="./target/release/unchained_browser",
    chrome_solver=cached_cookies_solver("cookies.json"),
)) as r:
    r.navigate("https://www.zillow.com/homes/for_rent/")  # auto-handles 403 + cookie replay
```

## RPC methods

| | |
|---|---|
| `navigate {url}` | fetch + parse + return `{status, url, bytes, blockmap, challenge}` |
| `query {selector}` | CSS query → `[{ref, tag, attrs, text}]` |
| `text {selector?}` | textContent of first match |
| `click {ref}` | dispatch click; auto-follows `<a href>` |
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

- **No page-script execution yet.** SPAs that render content client-side will return a JS shell. The blockmap exposes a `density.likely_js_filled` signal so agents can detect this in one call instead of burning round-trips.
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
