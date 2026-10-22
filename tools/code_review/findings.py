"""The finding model shared by the deterministic checks and the model pass."""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Any

SEVERITIES = ("critical", "major", "minor", "nit")
SEVERITY_RANK = {name: rank for rank, name in enumerate(SEVERITIES)}
SEVERITY_ICON = {"critical": "🔴", "major": "🟠", "minor": "🟡", "nit": "⚪"}


def normalize_severity(value: str | None) -> str:
    """Return a known severity, defaulting to ``minor``."""
    text = (value or "").strip().lower()
    return text if text in SEVERITY_RANK else "minor"


@dataclass
class Finding:
    """One review comment: what is wrong, where, and how sure we are."""

    file: str
    line: int | None
    severity: str
    category: str
    title: str
    body: str
    suggestion: str | None = None
    confidence: float = 0.8
    source: str = "llm"
    verified: str | None = None
    extra: dict[str, Any] = field(default_factory=dict)

    def sort_key(self) -> tuple[int, str, int]:
        return (SEVERITY_RANK.get(self.severity, 99), self.file, self.line or 0)

    def to_dict(self) -> dict[str, Any]:
        return {
            "file": self.file,
            "line": self.line,
            "severity": self.severity,
            "category": self.category,
            "title": self.title,
            "body": self.body,
            "suggestion": self.suggestion,
            "confidence": round(self.confidence, 2),
            "source": self.source,
            "verified": self.verified,
        }


_TOKEN_RE = re.compile(r"[a-z0-9]{3,}")


def _tokens(text: str) -> set[str]:
    return set(_TOKEN_RE.findall((text or "").lower()))


def _similar(left: Finding, right: Finding) -> bool:
    """Two findings are duplicates when they point at the same spot with a similar title."""
    if left.file != right.file:
        return False
    if left.line and right.line and abs(left.line - right.line) > 2:
        return False
    a, b = _tokens(left.title), _tokens(right.title)
    if not a or not b:
        return False
    return len(a & b) / len(a | b) >= 0.45


def merge_findings(findings: list[Finding]) -> list[Finding]:
    """Deduplicate rule/model overlap, keeping the strongest version of each finding."""
    kept: list[Finding] = []
    ordered = sorted(
        (f for f in findings if f.title.strip()),
        key=lambda f: (SEVERITY_RANK.get(f.severity, 99), -(f.confidence or 0.0), -len(f.body or "")),
    )
    for finding in ordered:
        duplicate = next((seen for seen in kept if _similar(seen, finding)), None)
        if duplicate is None:
            kept.append(finding)
            continue
        if finding.source not in duplicate.source.split("+"):
            duplicate.source = f"{duplicate.source}+{finding.source}"
        if len(finding.body or "") > len(duplicate.body or ""):
            duplicate.body = finding.body
        if not duplicate.suggestion and finding.suggestion:
            duplicate.suggestion = finding.suggestion
        duplicate.confidence = max(duplicate.confidence, finding.confidence)
    kept.sort(key=Finding.sort_key)
    return kept


def cap_findings(findings: list[Finding], limit: int) -> list[Finding]:
    if limit <= 0:
        return findings
    return sorted(findings, key=Finding.sort_key)[:limit]
