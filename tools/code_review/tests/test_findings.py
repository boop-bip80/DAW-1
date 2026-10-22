"""Tests for finding dedupe, ordering and capping."""

from __future__ import annotations

import unittest

from tools.code_review.findings import Finding, cap_findings, merge_findings, normalize_severity


def finding(line: int, title: str, severity: str = "major", source: str = "rule") -> Finding:
    return Finding(file="a.rs", line=line, severity=severity, category="c", title=title, body="body", source=source)


class MergeTests(unittest.TestCase):
    def test_near_duplicates_merge(self) -> None:
        merged = merge_findings(
            [
                finding(10, "Missing error handling in import", source="panic.unwrap"),
                finding(11, "Missing error handling on import path", source="llm"),
            ]
        )
        self.assertEqual(len(merged), 1)
        self.assertEqual(merged[0].source, "panic.unwrap+llm")

    def test_two_rule_hits_do_not_merge(self) -> None:
        merged = merge_findings(
            [
                finding(10, "`.unwrap()` can panic", source="panic.unwrap"),
                finding(11, "`.unwrap()` can panic", source="panic.unwrap"),
            ]
        )
        self.assertEqual(len(merged), 2)

    def test_different_locations_do_not_merge(self) -> None:
        merged = merge_findings([finding(10, "Missing error handling"), finding(30, "Missing error handling")])
        self.assertEqual(len(merged), 2)

    def test_strongest_severity_wins(self) -> None:
        merged = merge_findings([finding(10, "Same problem", "minor"), finding(10, "Same problem", "critical")])
        self.assertEqual(merged[0].severity, "critical")


class CapTests(unittest.TestCase):
    def test_keeps_most_severe(self) -> None:
        capped = cap_findings([finding(1, "a", "nit"), finding(2, "b", "critical"), finding(3, "c", "minor")], 2)
        self.assertEqual([item.severity for item in capped], ["critical", "minor"])

    def test_zero_means_no_cap(self) -> None:
        self.assertEqual(len(cap_findings([finding(1, "a")], 0)), 1)


class SeverityTests(unittest.TestCase):
    def test_unknown_defaults_to_minor(self) -> None:
        self.assertEqual(normalize_severity("URGENT"), "minor")
        self.assertEqual(normalize_severity(" Critical "), "critical")


if __name__ == "__main__":
    unittest.main()
