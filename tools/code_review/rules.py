"""Deterministic checks for the standards AGENTS.md calls non-negotiable.

These run without a model, so ``--offline`` still gives a useful gate. They are
deliberately narrow: every hit is a real violation of a documented rule, which
keeps the model pass focused on the semantic problems rules cannot see.
"""

from __future__ import annotations

import re
from dataclasses import dataclass

from .config import Config
from .diffparse import FileDiff
from .findings import Finding

# The only crates allowed to contain `unsafe`, and only in their FFI module.
ALLOWED_UNSAFE = {
    "crates/clap-host/src/ffi.rs",
    "crates/vst3-host/src/ffi.rs",
    "crates/au-host/src/ffi.rs",
}

ASSET_EXTENSIONS = {
    "png", "jpg", "jpeg", "svg", "ico", "icns", "ttf", "otf", "ttc", "woff", "woff2",
    "wav", "aif", "aiff", "flac", "mp3", "ogg", "gif", "webp",
}  # fmt: skip

# Block-comment continuation lines start with `*` followed by whitespace, a slash or nothing;
# a leading `*` before anything else is a dereference and must still reach the panic rules.
_COMMENT_LINE_RE = re.compile(r"^\s*(?://|/\*|\*(?:\s|/|$))")

_PANIC_RULES: tuple[tuple[str, re.Pattern[str], str, str, str, str], ...] = (
    (
        "panic.unwrap",
        re.compile(r"\.unwrap\(\)"),
        "critical",
        "crash",
        "`.unwrap()` can panic",
        "SoundCraft treats a crash as lost recordings. Propagate the error with `?`, or match on the value and return an \"unsupported\" error for unfinished work.",
    ),
    (
        "panic.expect",
        re.compile(r"\.expect\("),
        "critical",
        "crash",
        "`.expect()` can panic",
        "`expect` is banned in production code for the same reason as `unwrap`: the message does not make the process surviving. Handle the `Option`/`Result` explicitly.",
    ),
    (
        "panic.macro",
        re.compile(r"\bpanic!\s*\("),
        "critical",
        "crash",
        "`panic!` in production code",
        "Return a typed `Result` error instead. The engine catches escaped panics, but the document then has to be restored and the user loses the operation.",
    ),
    (
        "panic.unreachable",
        re.compile(r"\bunreachable!\s*\("),
        "critical",
        "crash",
        "`unreachable!()` in production code",
        "Model this state explicitly (an enum arm or an `unsupported` error) so hostile input cannot reach an abort.",
    ),
    (
        "panic.todo",
        re.compile(r"\btodo!\s*\("),
        "critical",
        "crash",
        "`todo!()` in production code",
        "An unfinished feature must return an \"unsupported\" error, never abort.",
    ),
    (
        "panic.unimplemented",
        re.compile(r"\bunimplemented!\s*\("),
        "critical",
        "crash",
        "`unimplemented!()` in production code",
        "An unfinished feature must return an \"unsupported\" error, never abort.",
    ),
)

_UNSAFE_RE = re.compile(r"\bunsafe\s*(\{|fn\b|impl\b|trait\b)")
_STATIC_MUT_RE = re.compile(r"\bstatic\s+mut\b")
_EXIT_RE = re.compile(r"\b(?:std::process::)?process::exit\s*\(")
_DBG_RE = re.compile(r"\bdbg!\s*\(")
_ALLOW_PANIC_RE = re.compile(
    r"(?:#!?\[)\s*allow\s*\([^]]*(?:unwrap_used|expect_used|clippy::panic|unreachable|todo|unimplemented)"
)
_ALLOW_UNSAFE_RE = re.compile(r"#!?\[\s*allow\s*\([^]]*unsafe_code")
_RT_PRINT_RE = re.compile(r"\b(?:println|eprintln|print|eprint)!\s*\(")


@dataclass(frozen=True)
class _RuleHit:
    rule_id: str
    severity: str
    category: str
    title: str
    body: str


def _test_module_start(lines: list[str]) -> int | None:
    for number, line in enumerate(lines, start=1):
        if line.strip().startswith("#[cfg(test)]"):
            return number
    return None


def _is_test_path(path: str) -> bool:
    return "/tests/" in f"/{path}" or path.startswith("tests/") or path.endswith("_test.rs")


def _severity(config: Config, rule_id: str, default: str) -> str:
    return config.rule_severity.get(rule_id, default)


def _line_rules(path: str, lines: list[str], index: int, config: Config) -> list[_RuleHit]:
    """Rules that look at a single added Rust line (``index`` is 0-based)."""
    text = lines[index]
    hits: list[_RuleHit] = []
    if _COMMENT_LINE_RE.match(text):
        return hits  # documented examples are not violations

    for rule_id, pattern, severity, category, title, body in _PANIC_RULES:
        if pattern.search(text):
            hits.append(_RuleHit(rule_id, _severity(config, rule_id, severity), category, title, body))

    allow_match = _ALLOW_PANIC_RE.search(text)
    if allow_match:
        hits.append(
            _RuleHit(
                "lint.allow_panic",
                _severity(config, "lint.allow_panic", "major"),
                "crash",
                "Crash lint suppressed",
                "`#![deny(clippy::unwrap_used, ...)]` exists so panics cannot creep back in. An `allow` here needs a comment explaining the invariant that makes the panic impossible.",
            )
        )

    unsafe_match = _UNSAFE_RE.search(text)
    if unsafe_match:
        allowed_file = path in ALLOWED_UNSAFE
        safety_comment = any("SAFETY:" in previous for previous in lines[max(0, index - 3) : index])
        if not allowed_file:
            hits.append(
                _RuleHit(
                    "unsafe.outside_host",
                    _severity(config, "unsafe.outside_host", "critical"),
                    "security",
                    "`unsafe` outside the isolated host crates",
                    "Unsafe code is confined to `clap-host`, `vst3-host` and `au-host` (their `src/ffi.rs`), each behind a safe `Result` API. Move the FFI here or avoid `unsafe`.",
                )
            )
        elif unsafe_match.group(1) == "{" and not safety_comment:
            hits.append(
                _RuleHit(
                    "unsafe.missing_safety",
                    _severity(config, "unsafe.missing_safety", "major"),
                    "security",
                    "`unsafe` block without a `// SAFETY:` comment",
                    "Every unsafe block in the FFI crates must state, immediately above it, why the preconditions hold (pointer validity, alignment, lifetime, aliasing).",
                )
            )

    if _ALLOW_UNSAFE_RE.search(text) and path not in ALLOWED_UNSAFE:
        hits.append(
            _RuleHit(
                "lint.allow_unsafe",
                _severity(config, "lint.allow_unsafe", "major"),
                "security",
                "`unsafe_code` lint suppressed outside the host crates",
                "`unsafe_code = \"forbid\"` is part of the crash-safety story. Add a host crate with the documented `deny` + `SAFETY` pattern instead of silencing the lint.",
            )
        )

    if _STATIC_MUT_RE.search(text):
        hits.append(
            _RuleHit(
                "unsafe.static_mut",
                _severity(config, "unsafe.static_mut", "major"),
                "security",
                "`static mut` shared mutable state",
                "`static mut` invites data races and requires `unsafe` at every access. Use a synchronization primitive with a safe API (or thread-local state) instead.",
            )
        )

    if _EXIT_RE.search(text) and path.startswith("crates/"):
        hits.append(
            _RuleHit(
                "process.exit",
                _severity(config, "process.exit", "major"),
                "correctness",
                "`process::exit` in a library crate",
                "Exiting from a library skips destructors and the engine's document recovery. Return an error to the app layer and let it decide how to exit.",
            )
        )

    if _DBG_RE.search(text):
        hits.append(
            _RuleHit(
                "debug.dbg",
                _severity(config, "debug.dbg", "minor"),
                "maintainability",
                "`dbg!` left in production code",
                "Remove the debug print, or use the `log`/`tracing` facility the crate already uses.",
            )
        )

    if _RT_PRINT_RE.search(text) and (path.startswith("crates/playback/src/") or path.startswith("crates/dsp/src/")):
        hits.append(
            _RuleHit(
                "realtime.print",
                _severity(config, "realtime.print", "minor"),
                "realtime",
                "Printing from DSP / playback code",
                "The audio callback must not block. Formatting and writing to stdio can lock and allocate; use the meter/telemetry path instead.",
            )
        )

    return hits


def _attribution_covers(path: str, attribution: str) -> bool:
    if f"`{path}`" in attribution:
        return True
    parts = path.split("/")
    for index in range(1, len(parts)):
        directory = "/".join(parts[:index])
        if f"`{directory}/`" in attribution:
            return True
    return False


def check(files: list[FileDiff], contents: dict[str, str], config: Config) -> list[Finding]:
    """Run every deterministic rule over the added lines of ``files``."""
    findings: list[Finding] = []
    attribution = contents.get("ATTRIBUTION.md", "")

    for file in files:
        if file.status == "deleted":
            continue

        if not file.binary:
            added = file.added_line_numbers()
            content = contents.get(file.path, "")
            lines = content.splitlines()
            added_text = file.added_line_text()
            test_start = _test_module_start(lines)
            test_path = _is_test_path(file.path)
            is_rust = file.path.endswith(".rs")

            for number in sorted(added):
                text = lines[number - 1] if 0 < number <= len(lines) else added_text.get(number, "")
                if not text.strip():
                    continue
                if test_path or (test_start is not None and number >= test_start):
                    continue  # tests may unwrap, panic and print
                if is_rust:
                    for hit in _line_rules(file.path, lines, number - 1, config):
                        findings.append(
                            Finding(
                                file=file.path,
                                line=number,
                                severity=hit.severity,
                                category=hit.category,
                                title=hit.title,
                                body=hit.body,
                                confidence=0.95,
                                source=hit.rule_id,
                            )
                        )

        if file.status == "added":
            extension = file.path.rsplit(".", 1)[-1].lower() if "." in file.path else ""
            if extension in ASSET_EXTENSIONS and not _attribution_covers(file.path, attribution):
                findings.append(
                    Finding(
                        file=file.path,
                        line=1,
                        severity=_severity(config, "assets.attribution", "major"),
                        category="assets",
                        title="Bundled asset missing an ATTRIBUTION.md row",
                        body=(
                            "Every bundled asset needs a row in ATTRIBUTION.md (path, author, source, license) in the same "
                            "commit that adds it; `cargo xtask assets` enforces this and CI fails otherwise."
                        ),
                        confidence=1.0,
                        source="assets.attribution",
                    )
                )

    return findings
