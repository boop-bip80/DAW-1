"""Tests for the deterministic SoundCraft rules."""

from __future__ import annotations

import unittest

from tools.code_review import rules
from tools.code_review.config import Config
from tools.code_review.diffparse import DiffLine, FileDiff, Hunk


def make_diff(path: str, lines: list[str], status: str = "added") -> FileDiff:
    hunk = Hunk(0, 0, 1, len(lines))
    for number, text in enumerate(lines, start=1):
        hunk.lines.append(DiffLine("+", text, None, number))
    return FileDiff(path=path, status=status, hunks=[hunk])


def check(path: str, lines: list[str], *, contents: dict[str, str] | None = None, config: Config | None = None) -> list:
    return rules.check([make_diff(path, lines)], contents if contents is not None else {path: "\n".join(lines)}, config or Config())


class PanicRuleTests(unittest.TestCase):
    def test_unwrap_is_critical(self) -> None:
        found = check("crates/model/src/lib.rs", ["pub fn f() -> i32 {", "    let x = maybe().unwrap();", "    x", "}"])
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0].severity, "critical")
        self.assertEqual(found[0].source, "panic.unwrap")
        self.assertEqual(found[0].line, 2)

    def test_expect_and_panic_macros(self) -> None:
        found = check("crates/engine/src/lib.rs", ["fn f(o: Option<u8>) {", "    let _ = o.expect(\"nope\");", "    panic!(\"boom\");", "}"])
        self.assertEqual({finding.source for finding in found}, {"panic.expect", "panic.macro"})

    def test_cfg_test_module_is_skipped(self) -> None:
        lines = ["pub fn f() {}", "#[cfg(test)]", "mod tests {", "    fn t() { Some(1).unwrap(); }", "}"]
        self.assertEqual(check("crates/model/src/lib.rs", lines), [])

    def test_tests_directory_is_skipped(self) -> None:
        self.assertEqual(check("crates/model/tests/roundtrip.rs", ["fn t() { Some(1).unwrap(); }"]), [])

    def test_comment_only_line_is_ignored(self) -> None:
        self.assertEqual(check("crates/model/src/lib.rs", ["/// Never call `.unwrap()` here.", "pub fn f() {}"]), [])

    def test_rule_severity_override(self) -> None:
        config = Config(rule_severity={"panic.unwrap": "nit"})
        found = check("crates/model/src/lib.rs", ["fn f() { a().unwrap(); }"], config=config)
        self.assertEqual(found[0].severity, "nit")


class UnsafeRuleTests(unittest.TestCase):
    def test_unsafe_outside_host_crates(self) -> None:
        found = check("crates/model/src/lib.rs", ["fn f() {", "    unsafe { do_it(); }", "}"])
        self.assertEqual([finding.source for finding in found], ["unsafe.outside_host"])
        self.assertEqual(found[0].severity, "critical")

    def test_unsafe_in_host_file_with_safety_comment(self) -> None:
        lines = ["// SAFETY: the caller guarantees a valid, aligned pointer.", "unsafe { *ptr }"]
        self.assertEqual(check("crates/clap-host/src/ffi.rs", lines), [])

    def test_unsafe_in_host_file_without_safety_comment(self) -> None:
        found = check("crates/clap-host/src/ffi.rs", ["unsafe { *ptr }"])
        self.assertEqual([finding.source for finding in found], ["unsafe.missing_safety"])
        self.assertEqual(found[0].severity, "major")

    def test_static_mut(self) -> None:
        found = check("crates/playback/src/lib.rs", ["static mut COUNTER: u64 = 0;"])
        self.assertEqual([finding.source for finding in found], ["unsafe.static_mut"])

    def test_allow_unsafe_outside_hosts(self) -> None:
        found = check("crates/dsp/src/lib.rs", ["#![allow(unsafe_code)]"])
        self.assertEqual([finding.source for finding in found], ["lint.allow_unsafe"])


class OtherRuleTests(unittest.TestCase):
    def test_allow_panic_lint(self) -> None:
        found = check("crates/model/src/lib.rs", ["#[allow(clippy::unwrap_used)]", "fn f() {}"])
        self.assertEqual([finding.source for finding in found], ["lint.allow_panic"])

    def test_process_exit_in_library_crate(self) -> None:
        found = check("crates/engine/src/lib.rs", ["fn f() { std::process::exit(1); }"])
        self.assertEqual([finding.source for finding in found], ["process.exit"])

    def test_dbg(self) -> None:
        found = check("crates/model/src/lib.rs", ["fn f() { dbg!(value); }"])
        self.assertEqual([finding.source for finding in found], ["debug.dbg"])

    def test_asset_without_attribution(self) -> None:
        found = check("assets/icons/new.png", [""], contents={"ATTRIBUTION.md": "# Attribution\n"})
        self.assertEqual([finding.source for finding in found], ["assets.attribution"])
        self.assertEqual(found[0].severity, "major")

    def test_asset_with_attribution_row(self) -> None:
        contents = {"ATTRIBUTION.md": "| `assets/icons/new.png` | me | CC0 | CC0 |"}
        self.assertEqual(check("assets/icons/new.png", [""], contents=contents), [])

    def test_asset_covered_by_parent_directory(self) -> None:
        contents = {"ATTRIBUTION.md": "| `assets/icons/` | me | CC0 | CC0 |"}
        self.assertEqual(check("assets/icons/new.png", [""], contents=contents), [])

    def test_binary_asset_without_attribution(self) -> None:
        diff = FileDiff(path="assets/icons/new.png", status="added", binary=True, hunks=[])
        found = rules.check([diff], {"ATTRIBUTION.md": "# Attribution\n"}, Config())
        self.assertEqual([finding.source for finding in found], ["assets.attribution"])
        self.assertEqual(found[0].severity, "major")

    def test_binary_file_skips_line_rules(self) -> None:
        diff = make_diff("crates/model/src/lib.rs", ["fn f() { a().unwrap(); }"])
        diff.binary = True
        contents = {"crates/model/src/lib.rs": "fn f() { a().unwrap(); }"}
        self.assertEqual(rules.check([diff], contents, Config()), [])


if __name__ == "__main__":
    unittest.main()
