"""Thin wrappers around the git plumbing the reviewer needs."""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from pathlib import Path


GIT_TIMEOUT_SECONDS = 300


class GitError(RuntimeError):
    """A git command that the reviewer needs failed."""


@dataclass
class Repo:
    root: Path

    @classmethod
    def discover(cls, start: Path | None = None) -> "Repo":
        try:
            completed = subprocess.run(
                ["git", "rev-parse", "--show-toplevel"],
                cwd=str(start or Path.cwd()),
                capture_output=True,
                text=True,
                timeout=GIT_TIMEOUT_SECONDS,
            )
        except subprocess.TimeoutExpired as exc:
            raise GitError(f"git rev-parse timed out after {GIT_TIMEOUT_SECONDS}s") from exc
        if completed.returncode != 0:
            raise GitError("not inside a git repository")
        return cls(Path(completed.stdout.strip()))

    def git(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        try:
            completed = subprocess.run(
                ["git", "-C", str(self.root), "-c", "core.quotepath=false", *args],
                capture_output=True,
                text=True,
                errors="replace",
                timeout=GIT_TIMEOUT_SECONDS,
            )
        except subprocess.TimeoutExpired as exc:
            raise GitError(f"git {' '.join(args)} timed out after {GIT_TIMEOUT_SECONDS}s") from exc
        if check and completed.returncode != 0:
            message = completed.stderr.strip() or completed.stdout.strip() or "git command failed"
            raise GitError(f"git {' '.join(args)}: {message}")
        return completed

    def try_git(self, *args: str) -> str | None:
        completed = self.git(*args, check=False)
        return completed.stdout if completed.returncode == 0 else None

    def rev(self, ref: str) -> str:
        out = self.git("rev-parse", "--verify", f"{ref}^{{commit}}", check=False)
        if out.returncode != 0:
            raise GitError(f"unknown revision: {ref}")
        return out.stdout.strip()

    def short(self, rev: str) -> str:
        return self.git("rev-parse", "--short", rev).stdout.strip()

    def merge_base(self, left: str, right: str) -> str | None:
        out = self.git("merge-base", left, right, check=False)
        return out.stdout.strip() if out.returncode == 0 else None

    def rev_list_count(self, span: str) -> int:
        out = self.git("rev-list", "--count", span, check=False).stdout.strip()
        try:
            return int(out)
        except ValueError:
            return 0

    def diff(self, *revs: str, cached: bool = False) -> str:
        args = [
            "diff",
            "--find-renames",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--unified=3",
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ]
        if cached:
            args.append("--cached")
        args.extend(revs)
        return self.git(*args).stdout

    def show(self, rev: str, path: str) -> str | None:
        completed = self.git("show", f"{rev}:{path}", check=False)
        return completed.stdout if completed.returncode == 0 else None

    def read_index(self, path: str) -> str | None:
        completed = self.git("show", f":{path}", check=False)
        return completed.stdout if completed.returncode == 0 else None

    def read_worktree(self, path: str) -> str | None:
        root = self.root.resolve()
        target = (self.root / path).resolve()
        if not target.is_relative_to(root):
            return None  # a symlink that leaves the repository is not part of the diff
        try:
            if target.stat().st_size > 2_000_000:
                return None
            return target.read_text(encoding="utf-8", errors="replace")
        except (OSError, ValueError):
            return None

    def untracked(self) -> list[str]:
        out = self.git("ls-files", "--others", "--exclude-standard", "-z", check=False).stdout
        return [name for name in out.split("\x00") if name.strip()]

    def tracked_changed_files(self, *revs: str, cached: bool = False) -> list[str]:
        args = ["diff", "--name-only", "--no-color", "--no-ext-diff"]
        if cached:
            args.append("--cached")
        args.extend(revs)
        out = self.git(*args).stdout
        return [line for line in out.splitlines() if line.strip()]
