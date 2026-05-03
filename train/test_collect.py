#!/usr/bin/env python3
"""Smoke test for train/collect.py — drives the binary against a tiny
in-memory corpus and asserts the on-disk artifacts come out shaped right.

This is a real-network test (the navigate hits example.com). Use the
NO_NETWORK_TESTS=1 env var to skip in CI environments that disallow
outbound traffic. Set UNBROWSER_BIN to point at a custom binary.

Run standalone:
  python3 train/test_collect.py

Or under unittest:
  python3 -m unittest train.test_collect
"""
from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "train"))

import collect  # noqa: E402  (path-modified to import sibling module)


def _binary_available() -> bool:
    return collect.bin_path().exists()


@unittest.skipIf(os.environ.get("NO_NETWORK_TESTS") == "1",
                 "NO_NETWORK_TESTS=1 set; skipping live-network smoke")
@unittest.skipUnless(_binary_available(),
                     f"unbrowser binary not at {collect.bin_path()}; "
                     "build with `cargo build --release` or set UNBROWSER_BIN")
class CollectSmokeTest(unittest.TestCase):
    """End-to-end smoke: tiny corpus -> collector -> well-shaped artifacts."""

    def test_one_site_collection(self):
        with tempfile.TemporaryDirectory(prefix="ub_collect_test_") as tmp:
            tmp_path = Path(tmp)
            corpus_path = tmp_path / "corpus.json"
            corpus_path.write_text(json.dumps([{
                "url": "https://example.com/",
                "category": "smoke",
                "expected_framework": "static_ssr",
                "notes": "tiny static page; ideal for harness smoke",
            }]))
            runs_dir = tmp_path / "runs"

            corpus = collect.load_corpus(corpus_path)
            self.assertEqual(len(corpus), 1)
            self.assertEqual(corpus[0]["url"], "https://example.com/")

            summary = collect.collect(
                corpus,
                binary=collect.bin_path(),
                runs_dir=runs_dir,
                concurrency=1,
                timeout_s=15.0,
                policy_blocklist=True,
                exec_scripts=True,
                retry_once=True,
            )

            # Output dir + per-site artifacts.
            self.assertTrue(runs_dir.exists())
            self.assertTrue((runs_dir / "_summary.json").exists())
            site_dir = runs_dir / "example.com"
            self.assertTrue(site_dir.exists(), msg=f"no per-site dir at {site_dir}")
            events_path = site_dir / "navigate.events.jsonl"
            self.assertTrue(events_path.exists(), msg=f"no events.jsonl at {events_path}")
            self.assertGreater(events_path.stat().st_size, 0,
                               msg="events.jsonl is empty")

            # Each line of events.jsonl should be valid JSON.
            with open(events_path) as f:
                events = [json.loads(line) for line in f if line.strip()]
            self.assertGreater(len(events), 0)

            result_path = site_dir / "result.json"
            self.assertTrue(result_path.exists())
            result = json.loads(result_path.read_text())
            self.assertIn("summary", result)
            self.assertEqual(result["summary"]["url"], "https://example.com/")

            # _summary.json shape.
            persisted = json.loads((runs_dir / "_summary.json").read_text())
            self.assertEqual(persisted["n_sites"], 1)
            self.assertEqual(persisted["concurrency"], 1)
            self.assertIn("outcomes", persisted)
            # example.com should always be reachable.
            self.assertEqual(persisted["outcomes"].get("ok", 0), 1,
                             msg=f"expected ok:1, got {persisted['outcomes']}")
            self.assertEqual(summary["ok"], 1)

    def test_seed_corpus_loads_and_categorises(self):
        """Sanity: the shipped seed corpus parses and has every category we promised."""
        seed = REPO / "train" / "corpus" / "seed_sites.json"
        self.assertTrue(seed.exists(), msg=f"missing seed corpus at {seed}")
        entries = collect.load_corpus(seed)
        self.assertGreaterEqual(len(entries), 100, msg="seed corpus too small")
        cats: dict[str, int] = {}
        for e in entries:
            self.assertIn("url", e)
            self.assertIn("category", e)
            cats[e["category"]] = cats.get(e["category"], 0) + 1
        # Each category in the spec should have ≥10 entries.
        required = {
            "static_news", "news_media", "marketplace_spa", "ecommerce",
            "developer_tools", "social_csr", "documentation",
            "media_streaming", "consumer_apps", "government_static",
        }
        for cat in required:
            self.assertIn(cat, cats, msg=f"category {cat} missing from seed corpus")
            self.assertGreaterEqual(cats[cat], 10,
                                    msg=f"category {cat} has only {cats[cat]} entries")


if __name__ == "__main__":
    unittest.main()
