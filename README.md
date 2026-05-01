# unchained_browser

> A single-binary, Chrome-free headless browser optimized for LLM agents. The cheap layer of a two-engine scraping stack — handles 80% of pages at 1/50th Chrome's per-session cost, with built-in handoff to real Chrome for the hard 20%.

**Open-source under Apache 2.0.** If you want a managed version that handles the Chrome-half escalation, fleet ops, residential IPs, and a built-in Claude agent loop, see **[unchainedsky.com](https://unchainedsky.com)** — same architecture, hosted.

```
                                       ┌───────────────────────────┐
LLM agent / MCP host / Python script ─►│  unchained_browser        │  ~50MB RAM
                                       │  • single binary          │  ~100ms startup
                                       │  • Chrome 131 TLS/H2 FP   │  zero config
                                       │  • BlockMap (~500 tokens) │  no Chrome needed
                                       │  • element refs + cookies │
                                       └─────────────┬─────────────┘
                                                     │
                                            on bot challenge ⇣
                                       ┌───────────────────────────┐
                                       │  real Chrome (CLI / CDP)  │  cookie handoff
                                       │  ─ used only when needed  │  cached for hours
                                       └───────────────────────────┘
```

## Why it exists

Most LLM agent web tasks fall into two cost tiers:

- **80% of pages:** static HTML, SSR pages, news, docs, classic forms. Real Chrome is overkill. curl is too dumb (no JS, no session). You burn tokens dumping HTML and write parsers by hand.
- **20% of pages:** SPAs, bot-walled sites, dynamic dashboards. Real Chrome is the right answer.

`unchained_browser` is the first tier. Single Rust binary, embedded QuickJS, real Chrome TLS fingerprint, JSON-RPC over stdio. When you do hit the 20%, the cookie-handoff router escalates cleanly without making your script structurally aware of the failure.

## When to use it

| Use case | This tool | curl/WebFetch | Real Chrome (Playwright/Browserless) |
|---|---|---|---|
| Static or SSR page | ✅ | ✅ but token-heavy | overkill |
| Multi-step nav with cookies | ✅ | painful | ✅ |
| Bot-walled (Cloudflare, PerimeterX) | ✅ with cookie handoff | ❌ blocked | ✅ |
| SPA whose data is JS-rendered | ❌ (until Phase 4/5) | ❌ | ✅ |
| LLM-shaped output (low tokens) | ✅ BlockMap | ❌ raw HTML | ❌ raw DOM |
| Run in Lambda / Workers / edge | ✅ single binary | ✅ | ❌ no Chrome |
| 100K+ pages/day cost | $0 | $0 | $$$ |

If most of your pages are SPAs, Playwright is the right tool today. If most of your pages are not, this is.

## 30-second tour

```bash
git clone <repo>
cd unchained_browser
cargo build --release         # ~2 min first time, single binary at target/release/unchained_browser

# Drive it: JSON-RPC over stdin/stdout, one line per request.
printf '%s\n' \
  '{"id":1,"method":"navigate","params":{"url":"https://news.ycombinator.com"}}' \
  '{"id":2,"method":"query","params":{"selector":".titleline > a"}}' \
  '{"id":3,"method":"close"}' \
  | ./target/release/unchained_browser
```

You get back: `navigate` returns a 200 + a low-token `BlockMap` (semantic page summary inline), `query` returns 30 stories with `{ref, tag, attrs, text}`, `close` exits.

## Three ways agents use it

### 1. MCP server (zero glue)

Add 4 lines to your MCP host config (Claude Desktop, Claude Code, Cursor, Cline):

```json
{
  "mcpServers": {
    "unchained": {
      "command": "/path/to/unchained_browser",
      "args": ["--mcp"]
    }
  }
}
```

12 tools auto-discovered: `navigate`, `query`, `text`, `click`, `type`, `submit`, `blockmap`, `body`, `eval`, `cookies_set`, `cookies_get`, `cookies_clear`. The agent sees them as native function-shaped tools.

### 2. Direct subprocess (custom agent runtimes)

```python
import subprocess, json
p = subprocess.Popen(["unchained_browser"], stdin=subprocess.PIPE,
                     stdout=subprocess.PIPE, text=True, bufsize=1)
i = 0
def call(method, **params):
    global i; i += 1
    p.stdin.write(json.dumps({"id": i, "method": method, "params": params}) + "\n")
    p.stdin.flush()
    return json.loads(p.stdout.readline())["result"]

result = call("navigate", url="https://news.ycombinator.com")
stories = call("query", selector=".titleline > a")
```

### 3. Auto-escalation router (Python wrapper for protected sites)

```python
from scripts.router import Router, RouterConfig, cached_cookies_solver

cfg = RouterConfig(
    binary="/path/to/unchained_browser",
    chrome_solver=cached_cookies_solver("/path/to/cookies.json"),
)
with Router(cfg) as r:
    # Same call shape — router handles 403 → escalate → retry transparently.
    result = r.navigate("https://www.zillow.com/homes/for_rent/")
    listings = r.query("article")
```

Behind the scenes: if the first navigate returns a `challenge` field (PerimeterX, Cloudflare, etc.), the router calls your `chrome_solver` callback to obtain cookies, replays them, and retries. Your code stays a one-liner.

## Worked example — Hacker News top 3 stories

```python
nav = call("navigate", url="https://news.ycombinator.com")
# nav['blockmap'] is a ~500-token page summary; the agent uses it to plan queries.

stories = call("query", selector=".titleline > a")
metadata = call("query", selector="tr.athing + tr span.score")  # adjacent sibling combinator

for story, meta in list(zip(stories, metadata))[:3]:
    print(f"{meta['text']:>12}  {story['text']}")
    print(f"             {story['attrs']['href']}")
```

3 RPC calls. ~600 tokens for the agent. No HTML parsing in your code. No regex.

## Worked example — bypassing PerimeterX (Zillow)

`navigate` to `zillow.com/homes/for_rent/` returns:

```json
{
  "status": 403,
  "bytes": 5816,
  "challenge": {
    "blocked": true,
    "provider": "perimeterx_block",
    "confidence": 0.94,
    "matched": ["px-captcha", "_pxappid"],
    "clearance_cookie": "_px3",
    "hint": "Solve once in real Chrome, copy _px3 via DevTools, paste with cookies_set, retry."
  }
}
```

The agent (or the router) handles this:

1. Solve the challenge once in real Chrome (e.g. via the [unchainedsky-cli](https://unchainedsky.com/cli) which drives real Chrome with stealth, or via Playwright, or via DevTools by hand).
2. Export the cookies.
3. Pipe them back via `cookies_set`.
4. Retry `navigate` — now returns 200 + 626KB of real listings.

The router automates this if you give it a `chrome_solver` callback. The cookie typically lasts 30 min – 24 h, so even at scale, real Chrome is invoked rarely.

## Available RPC methods (12)

| Method | Returns |
|---|---|
| `navigate {url}` | `{status, url, bytes, blockmap, challenge}` |
| `query {selector}` | `[{ref, tag, attrs, text}]` |
| `text {selector?}` | `string` (textContent of first match) |
| `blockmap` | semantic page summary (recompute) |
| `click {ref}` | dispatches click; auto-follows `<a href>` |
| `type {ref, text}` | sets value + dispatches input/change events |
| `submit {ref}` | gathers form fields, navigates to action URL (GET only) |
| `body` | raw HTML of last navigation (debugging) |
| `eval {code}` | runs JS in embedded QuickJS, returns JSON-stringified result |
| `cookies_set {cookies, url?}` | add cookies (objects or Set-Cookie strings) |
| `cookies_get` | list current cookies |
| `cookies_clear` | drop all |

CSS selector engine supports tag, id, class, attribute (`=`, `^=`, `$=`, `*=`, `~=`), all four combinators (` `, `>`, `+`, `~`), and `:first/last/nth-child/of-type`, `:only-child/of-type`. **Not yet:** `:not()`, `:has()`, `An+B` formulas in `nth-*`. Use `eval` for those.

## What it does *not* do (yet)

| Limitation | Status | Workaround |
|---|---|---|
| Run page scripts (SPAs, dynamic content) | Phase 4/5, not started | Use real Chrome via the cookie-handoff escalation, or escalate the entire navigate to Chrome |
| POST forms / multipart | Not yet | Construct the URL/POST manually via `eval` or escalate to Chrome |
| Beat advanced anti-bot (PerimeterX with behavioral telemetry, Akamai BMP advanced, Kasada) | Cookie handoff is the only path today; even Phase 4/5 won't beat the hardest tier reliably without residential IP | Real Chrome + residential proxy |
| Screenshots / pixel rendering | Out of scope by design | If you need vision, use real Chrome |
| `:not()`, `:has()`, complex CSS pseudos | Polish item | Drop into `eval` |

We document SPA detection so you bail cleanly instead of burning round-trips:

```python
nav = call("navigate", url="https://www.cnbc.com/markets/")
if nav["blockmap"]["density"]["likely_js_filled"]:
    # tables exist but are JS-filled — try the embedded JSON, or escalate
    json_blob = call("eval", code="document.querySelector('script[type=\"application/json\"]').textContent")
```

## Honest comparison

| | This tool | Playwright | Browserless / Browserbase | curl + parser |
|---|---|---|---|---|
| Engine | QuickJS (no JS exec yet) | real V8 | real V8 | none |
| RAM/session | ~50MB | 200–500MB | hosted | ~0 |
| Startup | ~100ms | ~1s | API call | instant |
| Bot detection (commodity) | ✅ Chrome TLS+H2 | ✅ | ✅ | ❌ |
| Bot detection (advanced) | needs cookie handoff | needs stealth tuning | hosted handles it | ❌ |
| SPA support | ❌ until Phase 4/5 | ✅ | ✅ | ❌ |
| Deploy footprint | one binary, ~10MB | needs Chrome (~250MB) | nothing local | nothing |
| Lambda / Workers | ✅ | ❌ | ✅ via API | ✅ |
| Per-page cost at 100K/day | $0 | infrastructure | API metered | $0 |
| LLM-shaped output | ✅ BlockMap inline | DIY | DIY or use their API | DIY |
| MCP-native | ✅ `--mcp` flag | wrapper required | wrapper required | wrapper required |

## How it works

```
┌─────────────────────────────────────────────────┐
│  unchained_browser (Rust)                       │
│                                                 │
│   ┌─────────┐    ┌──────────┐    ┌──────────┐   │
│   │ rquest  │───▶│html5ever │───▶│ rquickjs │   │
│   │ (Chrome │    │  parser  │    │ (QuickJS)│   │
│   │ TLS+H2) │    └──────────┘    │  +       │   │
│   └─────────┘                    │  dom.js  │   │
│       │                          │  +       │   │
│       ▼                          │ blockmap │   │
│   cookie_store ◀─────────────────│  +       │   │
│  (rquest-bound)                  │ interact │   │
│                                  └────┬─────┘   │
│                                       │         │
│   JSON-RPC stdin ──┐    ┌──── stdout  │         │
│                    ▼    ▲             ▼         │
│                 ┌───────────────────────┐       │
│                 │  Session dispatcher   │       │
│                 └───────────────────────┘       │
└─────────────────────────────────────────────────┘
```

- **`rquest`** sends Chrome-fingerprinted HTTPS requests (JA4 + Akamai H2 hash match Chrome 131).
- **`html5ever`** parses HTML into a tree on the host side.
- **`rquickjs`** embeds QuickJS; we seed it with `dom.js` (a static-DOM implementation), `blockmap.js` (the page-summary walker), and `interact.js` (click/type/submit primitives).
- **JSON-RPC** on stdin/stdout for commands; NDJSON events on stderr.
- **`cookie_store`** implements rquest's `CookieStore` trait, so cookies set via `cookies_set` flow into outgoing requests automatically.

Total: ~700 LOC Rust, ~1000 LOC JS (most of which is the ported DOM implementation).

## Roadmap

| Phase | Status | Effect |
|---|---|---|
| 1. Skeleton (JSON-RPC loop, eval) | ✅ | foundation |
| 2. Fetch + parse (rquest, html5ever) | ✅ | TLS-stealth navigate |
| 3. Virtual DOM (`query`/`text`) | ✅ | structured extraction |
| 4. Shims (navigator/canvas/WebGL/etc.) | next | foundation for Phase 5 |
| 5. Page-script execution | next | **closes the SPA gap** |
| 6. BlockMap + density signal | ✅ | LLM-shaped page summary |
| 7. Interactivity (click/type/submit) | ✅ | multi-step flows |
| 11a. Cookie jar | ✅ | session persistence |
| 12. MCP server mode | ✅ | one-config integration |
| 13. Challenge detector | ✅ | classify bot blocks |
| 14. Auto-escalation router | ✅ | transparent handoff |

Phase 4/5 is the structural unlock. After they land, the "✅" column above for SPA-heavy use cases inverts — most modern marketplace/dashboard sites become reachable without escalation.

## Skill for Claude Code

A pre-built skill ships at `~/.claude/skills/unchained-browser/SKILL.md`. Restart Claude Code (the watcher only watches dirs that existed at boot), then `/unchained-browser` becomes available, or describe a web task and the skill auto-triggers.

## Build deps

- Rust 1.95+ (rustup is fine)
- `cmake`, `ninja` (for BoringSSL via rquest's `boring-sys2`) — `brew install cmake ninja` on macOS
- ~2 min for first build (`boring-sys2` compiles BoringSSL once)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
brew install cmake ninja        # macOS
cargo build --release
```

## Self-host vs. managed

The open binary is the engine. Running it well in production means handling a few things you won't get for free:

| | This binary (open) | [unchainedsky.com](https://unchainedsky.com) (managed) |
|---|---|---|
| Cheap-path scraping | ✅ | ✅ |
| Chrome escalation for bot-walled / SPA pages | DIY: bring Playwright/CDP, run Chrome yourself, write the cookie loader | ✅ Chrome fleet, residential IPs, retry policies, all included |
| Cookie cache + rotation across workers | DIY: build a Redis/SQS layer | ✅ centrally managed, scoped per domain |
| Agent loop with Claude / DDM / Intel | DIY: stitch the Anthropic SDK + your own progress critic | ✅ [unchainedsky-cli](https://unchainedsky.com/cli) ships it |
| MCP server | ✅ via `--mcp` | ✅ remote MCP, no local install |
| Per-page cost at 100K/day | $0 (your infra) | usage-priced |
| Time-to-first-scrape | 2 min build + your wiring | one API key |
| You own the data | ✅ | ✅ |
| You own the ops | ✅ | nope, we do |

**Use the open binary when** you want full control, want to embed it inside your own product, are running edge/Lambda/Workers, or have an existing Chrome setup for escalation.

**Use [unchainedsky.com](https://unchainedsky.com) when** you'd rather not run a Chrome fleet, want SLA-backed scraping, want to skip the operational glue, or want the full agent loop wired up.

Either way: the JSON-RPC vocabulary and the BlockMap output shape are the same. Code you write against the open binary works against the hosted one and vice versa.

## License

Apache License 2.0 — see [LICENSE](./LICENSE). Permissive use commercially and otherwise; includes a patent grant covering the TLS-fingerprinting and challenge-detection bits.
