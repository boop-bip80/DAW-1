"""Tests for configuration loading and path globs."""

from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from tools.code_review.config import Config, ConfigError, glob_to_regex, load_config

TOML = """
[review]
base = "upstream/main"
fail_on = "critical"
max_findings = 7
exclude = ["target/**"]

[[review.path_instructions]]
path = "crates/engine/**"
instructions = "engine rules"

[llm]
base_url = "http://127.0.0.1:11434/v1"
model = "tiny-coder"
"""


class GlobTests(unittest.TestCase):
    def test_double_star(self) -> None:
        pattern = glob_to_regex("crates/**/*.rs")
        self.assertTrue(pattern.match("crates/a/b.rs"))
        self.assertFalse(pattern.match("crates/a/b.txt"))

    def test_single_star_does_not_cross_directories(self) -> None:
        pattern = glob_to_regex("crates/*/src/lib.rs")
        self.assertTrue(pattern.match("crates/model/src/lib.rs"))
        self.assertFalse(pattern.match("crates/model/nested/src/lib.rs"))


class LoadConfigTests(unittest.TestCase):
    def test_defaults(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            config = load_config(Path(directory))
        self.assertEqual(config, Config())
        self.assertEqual(config.fail_on, "major")
        self.assertTrue(config.verify)

    def test_file_values(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".code-review.toml").write_text(TOML, encoding="utf-8")
            config = load_config(root)
        self.assertEqual(config.base, "upstream/main")
        self.assertEqual(config.fail_on, "critical")
        self.assertEqual(config.max_findings, 7)
        self.assertEqual(config.model, "tiny-coder")
        self.assertTrue(config.excluded("target/debug/foo"))
        self.assertEqual(config.instructions_for("crates/engine/src/cmd/play.rs"), ["engine rules"])
        self.assertEqual(config.instructions_for("crates/model/src/lib.rs"), [])
        self.assertTrue(config.is_local_endpoint)

    def test_api_key_precedence(self) -> None:
        with mock.patch.dict(os.environ, {"REVIEW_API_KEY": "direct", "DEEPSEEK_API_KEY": "fallback"}, clear=False):
            with tempfile.TemporaryDirectory() as directory:
                config = load_config(Path(directory))
            self.assertEqual(config.api_key(), "direct")

    def test_environment_overrides(self) -> None:
        overrides = {"REVIEW_BASE": "main", "REVIEW_API_BASE": "http://127.0.0.1:8080/v1", "REVIEW_MODEL": "m"}
        with mock.patch.dict(os.environ, overrides, clear=False):
            with tempfile.TemporaryDirectory() as directory:
                config = load_config(Path(directory))
        self.assertEqual(config.base, "main")
        self.assertEqual(config.base_url, "http://127.0.0.1:8080/v1")
        self.assertEqual(config.model, "m")

    def test_invalid_fail_on(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".code-review.toml").write_text('[review]\nfail_on = "huge"\n', encoding="utf-8")
            with self.assertRaises(ConfigError):
                load_config(root)

    def test_explicit_missing_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ConfigError):
                load_config(Path(directory), Path(directory) / "missing.toml")


if __name__ == "__main__":
    unittest.main()
