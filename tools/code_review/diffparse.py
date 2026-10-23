"""Parse unified ``git diff`` output into files, hunks and numbered lines."""

from __future__ import annotations

import re
from dataclasses import dataclass, field

HUNK_RE = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
DIFF_GIT_RE = re.compile(r"^diff --git (?P<a>.+) (?P<b>.+)$")

_UNQUOTE = {
    "a": "\a",
    "b": "\b",
    "f": "\f",
    "n": "\n",
    "r": "\r",
    "t": "\t",
    "v": "\v",
    "\\": "\\",
    '"': '"',
}


@dataclass
class DiffLine:
    """One line of a hunk: ``+`` added, ``-`` removed, `` `` context."""

    kind: str
    text: str
    old_no: int | None = None
    new_no: int | None = None


@dataclass
class Hunk:
    old_start: int
    old_count: int
    new_start: int
    new_count: int
    lines: list[DiffLine] = field(default_factory=list)


@dataclass
class FileDiff:
    path: str
    old_path: str | None = None
    status: str = "modified"  # added | modified | deleted | renamed
    hunks: list[Hunk] = field(default_factory=list)
    binary: bool = False

    @property
    def added(self) -> int:
        return sum(1 for hunk in self.hunks for line in hunk.lines if line.kind == "+")

    @property
    def removed(self) -> int:
        return sum(1 for hunk in self.hunks for line in hunk.lines if line.kind == "-")

    def added_line_numbers(self) -> set[int]:
        return {line.new_no for hunk in self.hunks for line in hunk.lines if line.kind == "+" and line.new_no is not None}

    def added_line_text(self) -> dict[int, str]:
        return {
            line.new_no: line.text
            for hunk in self.hunks
            for line in hunk.lines
            if line.kind == "+" and line.new_no is not None
        }

    def changed_line_numbers(self) -> set[int]:
        """Added lines plus both sides of removed lines (useful for context lookups)."""
        numbers: set[int] = set()
        for hunk in self.hunks:
            for line in hunk.lines:
                if line.new_no is not None and line.kind != "-":
                    numbers.add(line.new_no)
                if line.old_no is not None and line.kind != "+":
                    numbers.add(line.old_no)
        return numbers


def unquote_path(value: str) -> str:
    """Undo git's C-style quoting for paths with spaces or unicode escapes."""
    if len(value) < 2 or not (value.startswith('"') and value.endswith('"')):
        return value
    body = value[1:-1]
    out = bytearray()
    index = 0
    while index < len(body):
        char = body[index]
        if char == "\\" and index + 1 < len(body):
            nxt = body[index + 1]
            if nxt in "01234567" and index + 3 < len(body):
                out.append(int(body[index + 1 : index + 4], 8))
                index += 4
                continue
            out.extend(_UNQUOTE.get(nxt, nxt).encode("utf-8"))
            index += 2
            continue
        out.extend(char.encode("utf-8"))
        index += 1
    return out.decode("utf-8", errors="replace")


def _strip_prefix(path: str) -> str:
    if path.startswith("a/") or path.startswith("b/"):
        return path[2:]
    return path


def _header_path(raw: str, marker: str) -> str | None:
    value = raw[len(marker) :].split("\t", 1)[0].strip()
    if value == "/dev/null":
        return None
    return _strip_prefix(unquote_path(value))


def parse(diff_text: str) -> list[FileDiff]:
    """Parse a unified diff into per-file hunks with stable old/new line numbers."""
    files: list[FileDiff] = []
    current: FileDiff | None = None
    hunk: Hunk | None = None
    old_no = 0
    new_no = 0

    for raw in diff_text.splitlines():
        if raw.startswith("diff --git "):
            match = DIFF_GIT_RE.match(raw)
            current = FileDiff(path="")
            if match:
                current.path = _strip_prefix(unquote_path(match.group("b")))
                current.old_path = _strip_prefix(unquote_path(match.group("a")))
            files.append(current)
            hunk = None
            continue

        if current is None:
            continue

        if raw.startswith("@@"):
            match = HUNK_RE.match(raw)
            if not match:
                continue
            hunk = Hunk(
                old_start=int(match.group(1)),
                old_count=int(match.group(2) or 1),
                new_start=int(match.group(3)),
                new_count=int(match.group(4) or 1),
            )
            current.hunks.append(hunk)
            old_no = hunk.old_start
            new_no = hunk.new_start
            continue

        if hunk is None:
            if raw.startswith("new file mode"):
                current.status = "added"
            elif raw.startswith("deleted file mode"):
                current.status = "deleted"
            elif raw.startswith("rename from "):
                current.old_path = unquote_path(raw[len("rename from ") :])
                current.status = "renamed"
            elif raw.startswith("rename to "):
                current.path = unquote_path(raw[len("rename to ") :])
                current.status = "renamed"
            elif raw.startswith("Binary files ") or raw.startswith("GIT binary patch"):
                current.binary = True
            elif raw.startswith("--- "):
                path = _header_path(raw, "--- ")
                if path is not None:
                    current.old_path = path
            elif raw.startswith("+++ "):
                path = _header_path(raw, "+++ ")
                if path is None:
                    current.status = "deleted"
                else:
                    current.path = path
            continue

        if raw.startswith("\\"):
            # "\ No newline at end of file"
            continue

        kind = raw[:1] or " "
        text = raw[1:] if raw else ""
        if kind == "+":
            hunk.lines.append(DiffLine("+", text, None, new_no))
            new_no += 1
        elif kind == "-":
            hunk.lines.append(DiffLine("-", text, old_no, None))
            old_no += 1
        elif kind == " ":
            hunk.lines.append(DiffLine(" ", text, old_no, new_no))
            old_no += 1
            new_no += 1

    for file in files:
        if file.status == "deleted" and file.old_path:
            file.path = file.old_path
    return [file for file in files if file.path]
