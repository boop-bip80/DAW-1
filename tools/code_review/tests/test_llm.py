"""Tests for JSON extraction from model replies."""

from __future__ import annotations

import unittest

from tools.code_review.llm import LLMError, estimate_tokens, extract_json


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


class TokenTests(unittest.TestCase):
    def test_estimate_is_positive(self) -> None:
        self.assertGreaterEqual(estimate_tokens("hello"), 1)
        self.assertGreater(estimate_tokens("x" * 3500), 900)


if __name__ == "__main__":
    unittest.main()
