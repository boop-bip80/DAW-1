"""Tests for window building, finding parsing and the report gate."""

from __future__ import annotations

import unittest

from tools.code_review.config import Config
from tools.code_review.diffparse import DiffLine, FileDiff, Hunk
from tools.code_review.findings import Finding
from tools.code_review.pipeline import (
    FileReview,
    ReviewResult,
    _parse_findings,
    _matches_filter,
    render_window,
    windows_for,
)


def make_review(lines: list[str], hunks: list[Hunk]) -> FileReview:
    return FileReview(diff=FileDiff(path="src/lib.rs", hunks=hunks), content="\n".join(lines))


class RenderWindowTests(unittest.TestCase):
    def test_marks_added_lines(self) -> None:
        lines = ["one", "two", "three"]
        rendered = render_window(lines, 1, 3, {2})
        self.assertIn(">>> 2 | two", rendered)
        self.assertIn("    1 | one", rendered)

    def test_out_of_range_lines_are_skipped(self) -> None:
        rendered = render_window(["only"], 1, 5, set())
        self.assertEqual(rendered.splitlines(), ["    1 | only"])


class WindowTests(unittest.TestCase):
    def test_merges_overlapping_hunks(self) -> None:
        lines = [f"line {number}" for number in range(1, 201)]
        first = Hunk(8, 2, 10, 2, [DiffLine("+", "line 10", None, 10), DiffLine("+", "line 11", None, 11)])
        second = Hunk(14, 1, 15, 1, [DiffLine("+", "line 15", None, 15)])
        windows = windows_for(make_review(lines, [first, second]), Config(context_lines=3))
        self.assertEqual(len(windows), 1)
        self.assertIn(">>> 10 | line 10", windows[0])
        self.assertIn("   7 | line 7", windows[0])
        self.assertIn("  19 | line 19", windows[0])
        self.assertNotIn("   6 | line 6", windows[0])
        self.assertNotIn("  20 | line 20", windows[0])

    def test_no_hunks_means_no_window(self) -> None:
        windows = windows_for(make_review(["a"], []), Config())
        self.assertEqual(windows, [])

    def test_binary_file_has_no_window(self) -> None:
        review = FileReview(diff=FileDiff(path="assets/x.png", binary=True), content="\x00")
        self.assertEqual(windows_for(review, Config()), [])


class ParseFindingTests(unittest.TestCase):
    def test_ignores_junk_and_clamps(self) -> None:
        data = {
            "findings": [
                "not a dict",
                {"title": "", "body": "x"},
                {"title": "Real", "body": "body", "line": "12", "severity": "HIGH", "confidence": 9},
            ]
        }
        findings = _parse_findings(data, "src/lib.rs")
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].severity, "minor")
        self.assertIsNone(findings[0].line)
        self.assertEqual(findings[0].confidence, 1.0)
        self.assertEqual(findings[0].file, "src/lib.rs")

    def test_accepts_positive_line(self) -> None:
        found = _parse_findings({"findings": [{"title": "t", "body": "b", "line": 3, "severity": "critical"}]}, "a.rs")
        self.assertEqual(found[0].line, 3)
        self.assertEqual(found[0].severity, "critical")


class ResultTests(unittest.TestCase):
    def test_gate_ignores_rejected(self) -> None:
        result = ReviewResult(base_ref="origin/main", base_rev=None, mode="branch")
        result.findings = [
            Finding(file="a.rs", line=1, severity="major", category="c", title="t", body="b", verified="rejected"),
            Finding(file="b.rs", line=2, severity="minor", category="c", title="t", body="b"),
        ]
        self.assertEqual(result.failing("major"), [])
        self.assertEqual(len(result.failing("minor")), 1)
        self.assertEqual(result.failing("none"), [])


class FilterTests(unittest.TestCase):
    def test_prefix_and_glob(self) -> None:
        self.assertTrue(_matches_filter("crates/engine/src/lib.rs", ["crates/engine"]))
        self.assertTrue(_matches_filter("crates/engine/src/lib.rs", ["crates/*/src/lib.rs"]))
        self.assertFalse(_matches_filter("crates/model/src/lib.rs", ["crates/engine"]))


if __name__ == "__main__":
    unittest.main()
