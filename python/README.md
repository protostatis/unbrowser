# unbrowser

**Web access for LLM agents. One static binary. No Chrome.**

Single-file native headless browser optimized for LLM agents. Runs JavaScript via QuickJS, returns a low-token page summary on every navigate, and gives you stable element refs for click/type/submit. Tens of MB RAM per session, no Chrome dependency.

```bash
pip install unbrowser
```

The wheel ships the native binary for your platform — there's nothing else to install.

## Quick start

```python
from unbrowser import Client

with Client() as ub:
    r = ub.navigate("https://news.ycombinator.com")
    for s in ub.query(".titleline > a")[:3]:
        print(s["text"], s["attrs"]["href"])
```

## Why

| | curl | This | Playwright / headless Chrome |
|---|---|---|---|
| Static / SSR pages | ✅ but token-heavy | ✅ low-token BlockMap | overkill |
| SPA-shell sites (with `exec_scripts`) | ❌ | ⚠️ partial | ✅ |
| Bot-walled (cookie handoff) | ❌ | ✅ | ✅ |
| Run in Lambda / Workers / edge | ✅ | ✅ | ❌ Chrome too big |
| Per-page cost at 100K/day | ~free | ~free | $$$ |
| LLM-shaped output | DIY parse | ✅ inline BlockMap | DIY parse |

## What it does

- **`navigate(url)`** — fetch, parse, return `{status, url, bytes, headers, blockmap, challenge}`. With `exec_scripts=True`, runs page JS in QuickJS (bounded by a 30s watchdog so it can't wedge).
- **`query(selector)`** — CSS query → `[{ref, tag, attrs, text}]`. Refs are stable handles for click/type/submit.
- **`extract()`** — auto-strategy structured data: tries JSON-LD → `__NEXT_DATA__` → Nuxt → OpenGraph → microdata → text fallback, returns highest-confidence hit.
- **`click(ref)` / `type(ref, text)` / `submit(ref)`** — interaction. POST and GET forms supported. Checkboxes/radios tracked.
- **`cookies_set(...)`** — paste cookies from a real Chrome session to bypass bot detection (Cloudflare, PerimeterX, Datadome). Solve once, replay forever.

Full RPC reference, BlockMap shape, challenge detection, profile system, and architecture: [github.com/protostatis/unbrowser](https://github.com/protostatis/unbrowser).

## Honest limits

- Heavy framework SPAs (Ember/React) often don't auto-mount inside QuickJS even with `exec_scripts=True` — the watchdog ensures it returns, check `density.likely_js_filled` to decide whether to escalate.
- No screenshots (out of scope by design).
- Hardest-tier anti-bot (FingerprintJS Pro, Kasada, advanced Akamai BMP) needs the cookie handoff path. The binary detects and labels the challenge for you.

## License

Apache-2.0.
