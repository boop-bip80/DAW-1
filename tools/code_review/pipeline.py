"""The review pipeline: collect the diff, run rules, ask the model, verify.

The shape mirrors a hosted reviewer: cheap deterministic checks first, then a
model pass over numbered windows of changed files, then an adversarial
verification pass so the report contains fewer false positives.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

from . import rules
from .config import Config, glob_to_regex
from .diffparse import DiffLine, FileDiff, Hunk, parse
from .findings import Finding, cap_findings, merge_findings, normalize_severity
from .gitio import GitError, Repo
from .llm import LLMClient, LLMError
from .prompts import SYSTEM_PROMPT, VERIFY_SYSTEM_PROMPT, build_review_prompt, build_verify_prompt

Progress = Callable[[str], None]


@dataclass
class FileReview:
    """A changed file plus the content the checks run against."""

    diff: FileDiff
    content: str

    @property
    def path(self) -> str:
        return self.diff.path


@dataclass
class ReviewResult:
    base_ref: str
    base_rev: str | None
    mode: str
    files: list[FileReview] = field(default_factory=list)
    findings: list[Finding] = field(default_factory=list)
    summaries: list[tuple[str, str]] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    llm_used: bool = False
    llm_windows: int = 0
    llm_errors: int = 0
    model: str = ""
    offline: bool = False

    @property
    def added(self) -> int:
        return sum(item.diff.added for item in self.files)

    @property
    def removed(self) -> int:
        return sum(item.diff.removed for item in self.files)

    def count(self, severity: str) -> int:
        return sum(1 for finding in self.findings if finding.severity == severity)

    def failing(self, fail_on: str) -> list[Finding]:
        from .findings import SEVERITY_RANK

        if fail_on not in SEVERITY_RANK:
            return []
        threshold = SEVERITY_RANK[fail_on]
        return [
            finding
            for finding in self.findings
            if SEVERITY_RANK.get(finding.severity, 99) <= threshold and finding.verified != "rejected"
        ]


def _matches_filter(path: str, patterns: list[str]) -> bool:
    if not patterns:
        return True
    for pattern in patterns:
        if any(char in pattern for char in "*?["):
            if glob_to_regex(pattern).match(path):
                return True
        elif path == pattern or path.startswith(pattern.rstrip("/") + "/"):
            return True
    return False


def _looks_binary(content: str) -> bool:
    return "\x00" in content[:8192]


def _reconstruct(diff: FileDiff) -> str:
    """Rebuild the new side of a file from the diff when no blob is available."""
    highest = 0
    for hunk in diff.hunks:
        for line in hunk.lines:
            if line.new_no is not None:
                highest = max(highest, line.new_no)
    if highest == 0:
        return ""
    lines = [""] * highest
    for hunk in diff.hunks:
        for line in hunk.lines:
            if line.new_no is not None and 0 < line.new_no <= highest:
                lines[line.new_no - 1] = line.text
    return "\n".join(lines)


def _untracked_diff(repo: Repo, path: str) -> FileDiff | None:
    content = repo.read_worktree(path)
    if content is None:
        return None
    diff = FileDiff(path=path, status="added", binary=_looks_binary(content))
    if diff.binary:
        return diff
    lines = content.splitlines()
    hunk = Hunk(0, 0, 1, len(lines))
    for number, text in enumerate(lines, start=1):
        hunk.lines.append(DiffLine("+", text, None, number))
    diff.hunks.append(hunk)
    return diff


def _read_content(repo: Repo, path: str, mode: str, base_rev: str | None, diff: FileDiff) -> str:
    content: str | None = None
    if mode == "staged":
        content = repo.read_index(path)
    content = content if content is not None else repo.read_worktree(path)
    if content is None and mode == "staged":
        content = repo.show("HEAD", path)
    if content is None and base_rev and diff.status != "added":
        content = repo.show(base_rev, path)
    return content if content is not None else _reconstruct(diff)


def _collect(
    repo: Repo,
    config: Config,
    base: str | None,
    mode: str,
    paths: list[str],
    progress: Progress,
) -> tuple[str, str | None, list[FileReview]]:
    base_ref = base or config.base
    base_rev: str | None = None
    if mode == "branch":
        try:
            base_rev = repo.rev(base_ref)
        except GitError as exc:
            raise GitError(f"cannot resolve base {base_ref!r}: {exc}. Fetch it or pass --base.") from exc
        merge_base = repo.merge_base(base_rev, "HEAD") or base_rev
        diff_text = repo.diff(merge_base)
        base_rev = merge_base
    elif mode == "working":
        base_ref = "HEAD"
        base_rev = repo.rev("HEAD")
        diff_text = repo.diff("HEAD")
    elif mode == "staged":
        base_ref = "HEAD"
        base_rev = repo.rev("HEAD")
        diff_text = repo.diff(cached=True)
    else:
        raise ValueError(f"unknown mode: {mode}")

    diffs = parse(diff_text)
    selected: list[FileDiff] = []
    for diff in diffs:
        if diff.status == "deleted":
            continue
        if diff.old_path is None and diff.path.endswith("/"):
            continue
        if config.excluded(diff.path) or not _matches_filter(diff.path, paths):
            continue
        selected.append(diff)

    if config.include_untracked and mode in {"branch", "working"}:
        known = {diff.path for diff in selected}
        for path in repo.untracked():
            if path in known or config.excluded(path) or not _matches_filter(path, paths):
                continue
            diff = _untracked_diff(repo, path)
            if diff is not None:
                selected.append(diff)

    files: list[FileReview] = []
    for diff in selected:
        content = _read_content(repo, diff.path, mode, base_rev, diff)
        if _looks_binary(content):
            diff.binary = True
        files.append(FileReview(diff=diff, content=content))

    progress(f"collected {len(files)} changed file(s) against {base_ref}")
    return base_ref, base_rev, files


def render_window(lines: list[str], start: int, end: int, added: set[int]) -> str:
    """Numbered lines; ``>>>`` marks added lines so the model only comments on them."""
    width = len(str(end))
    out: list[str] = []
    for number in range(start, end + 1):
        if not 0 < number <= len(lines):
            continue
        marker = ">>>" if number in added else "   "
        out.append(f"{marker} {number:>{width}} | {lines[number - 1]}")
    return "\n".join(out)


def _split_rendered(rendered: str, max_chars: int) -> list[str]:
    if len(rendered) <= max_chars:
        return [rendered]
    chunks: list[str] = []
    current: list[str] = []
    size = 0
    for line in rendered.splitlines():
        if current and size + len(line) + 1 > max_chars:
            chunks.append("\n".join(current))
            current, size = [], 0
        current.append(line)
        size += len(line) + 1
    if current:
        chunks.append("\n".join(current))
    return chunks


def windows_for(review: FileReview, config: Config) -> list[str]:
    """Build numbered context windows around the hunks, split to the token budget."""
    diff = review.diff
    if diff.binary:
        return []
    lines = review.content.splitlines()
    added = diff.added_line_numbers()
    ranges: list[tuple[int, int]] = []
    for hunk in diff.hunks:
        first = max(1, hunk.new_start - config.context_lines)
        last = min(len(lines), hunk.new_start + max(hunk.new_count, 1) + config.context_lines)
        if last < first:
            continue
        if ranges and first <= ranges[-1][1] + 1:
            ranges[-1] = (ranges[-1][0], max(ranges[-1][1], last))
        else:
            ranges.append((first, last))
    if not ranges and added:
        lowest, highest = min(added), max(added)
        ranges = [(max(1, lowest - config.context_lines), min(len(lines), highest + config.context_lines))]
    max_chars = max(4000, int(config.max_input_tokens * 3.5) - 1600)
    windows: list[str] = []
    for start, end in ranges:
        rendered = render_window(lines, start, end, added)
        windows.extend(_split_rendered(rendered, max_chars))
    return windows


def _parse_findings(data: dict, path: str) -> list[Finding]:
    raw_findings = data.get("findings")
    if not isinstance(raw_findings, list):
        return []
    findings: list[Finding] = []
    for raw in raw_findings:
        if not isinstance(raw, dict):
            continue
        title = str(raw.get("title") or "").strip()
        body = str(raw.get("body") or "").strip()
        if not title or not body:
            continue
        line = raw.get("line")
        if isinstance(line, bool) or not isinstance(line, int) or line <= 0:
            line = None
        confidence = raw.get("confidence", 0.6)
        if not isinstance(confidence, (int, float)) or isinstance(confidence, bool):
            confidence = 0.6
        suggestion = raw.get("suggestion")
        findings.append(
            Finding(
                file=path,
                line=line,
                severity=normalize_severity(str(raw.get("severity") or "")),
                category=str(raw.get("category") or "review").strip() or "review",
                title=title[:160],
                body=body,
                suggestion=str(suggestion).strip() if suggestion else None,
                confidence=max(0.0, min(1.0, float(confidence))),
                source="llm",
            )
        )
    return findings


def _review_file(
    client: LLMClient, review: FileReview, config: Config, warnings: list[str]
) -> tuple[str, list[Finding], int, int]:
    if not review.diff.added_line_numbers():
        return "", [], 0, 0
    windows = windows_for(review, config)
    if not windows:
        return "", [], 0, 0
    instructions = config.instructions_for(review.path)
    summary = ""
    findings: list[Finding] = []
    errors = 0
    for index, window in enumerate(windows):
        prompt = build_review_prompt(
            review.path,
            window,
            instructions=instructions,
            is_new_file=review.diff.status == "added",
            max_chars=max(2000, int(config.max_input_tokens * 3.5) - 1200),
        )
        try:
            data = client.chat_json(SYSTEM_PROMPT, prompt, max_tokens=2500)
        except LLMError as exc:
            errors += 1
            warnings.append(f"{review.path}: model pass failed ({exc})")
            continue
        if not summary:
            summary = str(data.get("summary") or "").strip()
        findings.extend(_parse_findings(data, review.path))
        if len(windows) > 1:
            warnings.append(f"{review.path}: reviewed in {len(windows)} windows")
    return summary, findings, len(windows), errors


def verify(client: LLMClient, findings: list[Finding], files: dict[str, FileReview], config: Config, warnings: list[str]) -> list[Finding]:
    """Ask the model to confirm or reject its own findings against the file text."""
    by_file: dict[str, list[Finding]] = {}
    for finding in findings:
        by_file.setdefault(finding.file, []).append(finding)

    confirmed: list[Finding] = []
    for path, group in by_file.items():
        review = files.get(path)
        if review is None:
            continue
        lines = review.content.splitlines()
        for start in range(0, len(group), 10):
            chunk = group[start : start + 10]
            numbers = [finding.line for finding in chunk if finding.line]
            if numbers:
                low = max(1, min(numbers) - 30)
                high = min(len(lines), max(numbers) + 30)
            else:
                low, high = 1, min(len(lines), 80)
            window = render_window(lines, low, high, review.diff.added_line_numbers())
            payload = [finding.to_dict() for finding in chunk]
            prompt = build_verify_prompt(path, window, payload)
            try:
                data = client.chat_json(VERIFY_SYSTEM_PROMPT, prompt, max_tokens=1500)
            except LLMError as exc:
                warnings.append(f"{path}: verification failed ({exc}); findings kept unverified")
                confirmed.extend(chunk)
                continue
            results = data.get("results")
            verdicts: dict[int, str] = {}
            if isinstance(results, list):
                for item in results:
                    if not isinstance(item, dict):
                        continue
                    index = item.get("index")
                    verdict = str(item.get("verdict") or "").strip().lower()
                    if isinstance(index, int) and 0 <= index < len(chunk) and verdict in {"confirmed", "rejected", "uncertain"}:
                        verdicts[index] = verdict
            for index, finding in enumerate(chunk):
                verdict = verdicts.get(index)
                if verdict == "rejected":
                    continue
                finding.verified = verdict or "uncertain"
                if verdict == "uncertain":
                    finding.confidence = min(finding.confidence, 0.5)
                confirmed.append(finding)
    return confirmed


def run(
    repo: Repo,
    config: Config,
    *,
    base: str | None = None,
    mode: str = "branch",
    paths: list[str] | None = None,
    use_llm: bool = True,
    progress: Progress | None = None,
) -> ReviewResult:
    """Run the full pipeline and return the merged, capped findings."""
    notify = progress or (lambda message: None)
    base_ref, base_rev, files = _collect(repo, config, base, mode, list(paths or []), notify)

    result = ReviewResult(
        base_ref=base_ref,
        base_rev=base_rev,
        mode=mode,
        files=files,
        model=config.model,
        offline=not use_llm,
    )
    contents = {item.path: item.content for item in files}
    if "ATTRIBUTION.md" not in contents:
        attribution = repo.read_worktree("ATTRIBUTION.md")
        if attribution is None:
            attribution = repo.show("HEAD", "ATTRIBUTION.md") or ""
        contents["ATTRIBUTION.md"] = attribution
    findings = rules.check([item.diff for item in files], contents, config)
    notify(f"deterministic rules: {len(findings)} finding(s)")

    client = LLMClient(config)
    if use_llm:
        if not client.available:
            result.warnings.append(
                f"model pass skipped: no API key ({config.api_key_env} or REVIEW_API_KEY) and "
                f"{config.base_url} is not a local endpoint"
            )
        else:
            reviewed = 0
            llm_findings: list[Finding] = []
            for item in files:
                if not item.diff.added_line_numbers():
                    continue
                reviewed += 1
                notify(f"reviewing {item.path} ({reviewed}/{len(files)})")
                summary, found, windows, errors = _review_file(client, item, config, result.warnings)
                result.llm_windows += windows
                result.llm_errors += errors
                if summary:
                    result.summaries.append((item.path, summary))
                llm_findings.extend(found)
            result.llm_used = reviewed > 0
            notify(f"model pass: {len(llm_findings)} finding(s)")
            if config.verify and llm_findings:
                notify("verifying findings")
                llm_findings = verify(client, llm_findings, {item.path: item for item in files}, config, result.warnings)
            findings.extend(llm_findings)

    result.findings = cap_findings(merge_findings(findings), config.max_findings)
    notify(f"{len(result.findings)} finding(s) after merge and cap")
    return result


def relative_files(repo: Repo) -> list[str]:
    """Convenience for callers that need the tracked file list."""
    out = repo.git("ls-files", check=False).stdout
    return [line for line in out.splitlines() if line.strip()]


def review_root(path: str | Path) -> Repo:
    return Repo.discover(Path(path))
