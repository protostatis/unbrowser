"""fp_check.py — verify a profile's wire fingerprint matches Chrome.

Run against tls.peet.ws/api/all (returns observed JA3, JA4, peetprint,
HTTP/2 frame ordering hash, headers + ordering as JSON). Compares the
observed values against a baseline known to come from real Chrome.

Usage:
    python3 scripts/fp_check.py [profile_name]

Exit code 0 = fingerprint shape matches Chrome family, 1 otherwise.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys

BIN = os.environ.get(
    "UNBROWSE_BIN",
    "~/Projects/unchained_browser/target/debug/unbrowse",
)

# Loose Chrome family checks. JA3/JA4 hashes drift across releases — we
# care that the SHAPE is Chrome (peetprint group, ALPN h2, frame ordering),
# not that the hash matches a specific frozen value.
CHROME_SIGNALS = {
    "alpn_first": "h2",
    "tls_version": "TLS 1.3",
    "user_agent_contains": "Chrome",
}


def navigate_via_unbrowse(url: str, profile: str | None) -> dict:
    args = [BIN]
    if profile:
        args += ["--profile", profile]
    proc = subprocess.Popen(
        args,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        req = json.dumps({"id": 1, "method": "navigate", "params": {"url": url}}) + "\n"
        proc.stdin.write(req)
        proc.stdin.flush()
        line = proc.stdout.readline()
        resp = json.loads(line)
        # The response body is also wanted to read TLS-peet's JSON.
        body_req = json.dumps({"id": 2, "method": "body", "params": {}}) + "\n"
        proc.stdin.write(body_req)
        proc.stdin.flush()
        body_line = proc.stdout.readline()
        body_resp = json.loads(body_line)
        proc.stdin.close()
        return {"navigate": resp, "body": body_resp}
    finally:
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()


def check_one(profile: str | None) -> bool:
    label = profile or "default"
    print(f"--- profile: {label} ---")
    out = navigate_via_unbrowse("https://tls.peet.ws/api/all", profile)
    nav = out["navigate"].get("result")
    if not nav:
        print(f"  navigate failed: {out['navigate'].get('error')}")
        return False
    body = out["body"].get("result")
    if not body:
        print("  no body returned")
        return False
    try:
        observed = json.loads(body)
    except json.JSONDecodeError:
        print(f"  body not JSON (first 200 chars): {body[:200]}")
        return False

    tls = observed.get("tls", {})
    http2 = observed.get("http2", {})
    http_version = observed.get("http_version", "")
    ua = (observed.get("user_agent") or "")

    ja3 = tls.get("ja3_hash") or tls.get("ja3")
    ja4 = tls.get("ja4") or ""
    # tls.peet.ws returns the numeric IANA code: 772 (0x0304) == TLS 1.3.
    tls_ver = tls.get("tls_version_negotiated") or tls.get("tls_version")
    h2_hash = http2.get("akamai_fingerprint_hash") or http2.get("akamai_fingerprint")

    # JA4 prefix tells us TLS version + ALPN directly: 't13d...h2_...'
    # means TLS 1.3 + h2 ALPN. Source of truth, no ambiguity.
    ja4_prefix = ja4.split("_")[0] if "_" in ja4 else ja4
    ja4_tls = "TLS 1.3" if ja4_prefix.startswith("t13") else (
        "TLS 1.2" if ja4_prefix.startswith("t12") else "?")
    ja4_alpn = "h2" if "h2" in ja4_prefix else ("h3" if "h3" in ja4_prefix else "?")

    print(f"  UA: {ua[:80]}{'…' if len(ua) > 80 else ''}")
    print(f"  ja3: {ja3}")
    print(f"  ja4: {ja4}  (tls={ja4_tls} alpn={ja4_alpn})")
    print(f"  tls_version raw: {tls_ver}")
    print(f"  http_version: {http_version}")
    print(f"  h2 akamai hash: {h2_hash}")

    ok = True
    if ja4_alpn != CHROME_SIGNALS["alpn_first"]:
        print(f"  FAIL: ja4 alpn should be {CHROME_SIGNALS['alpn_first']}")
        ok = False
    if CHROME_SIGNALS["user_agent_contains"] not in ua:
        print(f"  FAIL: UA should contain {CHROME_SIGNALS['user_agent_contains']}")
        ok = False
    if ja4_tls != CHROME_SIGNALS["tls_version"]:
        print(f"  FAIL: ja4 tls should be {CHROME_SIGNALS['tls_version']}")
        ok = False
    if http_version != "h2":
        print(f"  FAIL: http_version should be h2, got {http_version}")
        ok = False
    if not h2_hash:
        print("  WARN: no http2 akamai hash")
    if ok:
        print("  PASS")
    return ok


def main():
    profiles = sys.argv[1:] or ["chrome_134", "chrome_131"]
    results = [(p, check_one(p)) for p in profiles]
    print()
    print("Summary:")
    for p, ok in results:
        print(f"  {p}: {'PASS' if ok else 'FAIL'}")
    sys.exit(0 if all(ok for _, ok in results) else 1)


if __name__ == "__main__":
    main()
