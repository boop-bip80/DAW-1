"""Tests for JSON extraction from model replies."""

from __future__ import annotations

import os
import unittest
from unittest import mock

from tools.code_review.config import Config
from tools.code_review.llm import LLMClient, LLMError, _NoRedirect, estimate_tokens, extract_json


class ExtractJsonTests(unittest.TestCase):
    def test_plain_object(self) -> None:
        self.assertEqual(extract_json('{"findings": []}'), {"findings": []})

    def test_markdown_fence(self) -> None:
        self.assertEqual(extract_json('```json\n{"a": 1}\n```'), {"a": 1})

    def test_prose_around_object(self) -> None:
        self.assertEqual(extract_json('Sure! Here it is:\n{"a": {"b": 2}}\nAnything else?'), {"a": {"b": 2}})

    def test_braces_inside_strings(self) -> None:
        self.assertEqual(extract_json('{"body": "use {braces} carefully"}'), {"body": "use {braces} carefully"})

    def test_no_json_raises(self) -> None:
        with self.assertRaises(LLMError):
            extract_json("I could not review this.")


class EndpointTests(unittest.TestCase):
    def test_key_is_not_sent_over_remote_http(self) -> None:
        config = Config(base_url="http://example.com/v1", api_key_env="REVIEW_TEST_KEY")
        with mock.patch.dict(os.environ, {"REVIEW_TEST_KEY": "secret"}):
            with self.assertRaisesRegex(LLMError, "non-loopback HTTP"):
                LLMClient(config).chat("system", "user")

    def test_non_http_endpoint_is_refused(self) -> None:
        config = Config(base_url="ftp://example.com/v1", api_key_env="REVIEW_TEST_KEY")
        with mock.patch.dict(os.environ, {"REVIEW_TEST_KEY": "secret"}):
            with self.assertRaisesRegex(LLMError, "must be an http"):
                LLMClient(config).chat("system", "user")


class RedirectTests(unittest.TestCase):
    def test_redirects_are_refused(self) -> None:
        handler = _NoRedirect()
        with self.assertRaisesRegex(LLMError, "redirected"):
            handler.redirect_request(None, None, 302, "Found", {}, "https://elsewhere.example/v1")


class TokenTests(unittest.TestCase):
    def test_estimate_is_positive(self) -> None:
        self.assertGreaterEqual(estimate_tokens("hello"), 1)
        self.assertGreater(estimate_tokens("x" * 3500), 900)


if __name__ == "__main__":
    unittest.main()
