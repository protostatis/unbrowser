"""watch.py — pretty-print the unbrowser NDJSON event stream.

The binary emits one JSON event per line on stderr. This reads them and
renders a color-coded summary so a developer or operator can see at a
glance what the agent is doing across navigations, challenges, and
script executions — without parsing raw JSON.

Usage:

    # Pipe a session's stderr through the watcher
    /path/to/unbrowser 2> >(python3 scripts/watch.py)

    # Or tail an existing log
    cat session.log | python3 scripts/watch.py
    tail -f session.log | python3 scripts/watch.py

Designed for terminal use; falls back to plain text if stdout isn't a tty
(so it stays useful inside tmux/CI/log files).
"""

from __future__ import annotations

import json
import sys
from datetime import datetime


# ANSI codes only when stdout is a tty.
_USE_COLOR = sys.stdout.isatty()


def c(code: str, s: str) -> str:
    return f"\033[{code}m{s}\033[0m" if _USE_COLOR else s


def fmt_event(name: str, fields: dict) -> str:
    ts = datetime.now().strftime("%H:%M:%S")

    if name == "ready":
        return (
            f"{c('90', ts)}  {c('32;1', 'READY')}  "
            f"v{fields.get('version', '?')}  "
            f"profile={fields.get('profile', '?')}  "
            f"budget={fields.get('dispatch_budget_ms', '?')}ms"
        )

    if name == "navigate":
        status = fields.get("status", 0)
        # Color status: 2xx green, 3xx cyan, 4xx yellow, 5xx red.
        if 200 <= status < 300:
            status_str = c("32", str(status))
        elif 300 <= status < 400:
            status_str = c("36", str(status))
        elif 400 <= status < 500:
            status_str = c("33", str(status))
        else:
            status_str = c("31;1", str(status))
        url = fields.get("url", "?")
        if len(url) > 80:
            url = url[:77] + "…"
        bits = [
            f"{c('90', ts)}",
            f"  {c('34;1', 'NAV')}    ",
            f"{status_str} ",
            f"{fields.get('elapsed_ms', '?')}ms ",
            f"{fields.get('bytes', 0)}B ",
            url,
        ]
        if fields.get("exec_scripts"):
            ex = fields.get("scripts_executed") or 0
            it = fields.get("scripts_interrupted") or 0
            bits.append(f"  scripts:{c('35', str(ex))}")
            if it:
                bits.append(f" {c('31', f'interrupted:{it}')}")
        return "".join(bits)

    if name == "challenge":
        prov = fields.get("provider", "unknown")
        conf = fields.get("confidence", 0)
        cookie = fields.get("clearance_cookie") or "—"
        return (
            f"{c('90', ts)}  {c('33;1', 'CHALLENGE')}  "
            f"{c('31', prov)}  conf={conf:.2f}  "
            f"clearance_cookie={cookie}"
        )

    # Unknown event — show the JSON unmolested so we don't lose info on a
    # newly-added event the formatter doesn't know about yet.
    return f"{c('90', ts)}  {c('37', name.upper())}  {json.dumps(fields, separators=(',', ':'))}"


def main() -> int:
    try:
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                # Surface non-JSON lines too — sometimes useful for stderr noise.
                print(line, flush=True)
                continue
            name = obj.get("event", "?")
            fields = obj.get("data") or {}
            print(fmt_event(name, fields), flush=True)
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    sys.exit(main())
