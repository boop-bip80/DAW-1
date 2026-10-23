"""Tests for the git plumbing helpers (quoting, symlinks, repository discovery)."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path

from tools.code_review.gitio import GitError, Repo


class RepoTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True, capture_output=True)
        self.repo = Repo(self.root)

    def test_reads_a_worktree_file(self) -> None:
        (self.root / "note.txt").write_text("hello", encoding="utf-8")
        self.assertEqual(self.repo.read_worktree("note.txt"), "hello")

    def test_refuses_a_symlink_outside_the_repository(self) -> None:
        with tempfile.TemporaryDirectory() as outside_directory:
            target = Path(outside_directory) / "secret.txt"
            target.write_text("secret", encoding="utf-8")
            (self.root / "escape.txt").symlink_to(target)
            self.assertIsNone(self.repo.read_worktree("escape.txt"))

    def test_untracked_paths_keep_spaces_and_non_ascii(self) -> None:
        names = ["a file.txt", "中文.txt"]
        for name in names:
            (self.root / name).write_text("x", encoding="utf-8")
        self.assertEqual(sorted(self.repo.untracked()), sorted(names))

    def test_discover_outside_a_repository_fails(self) -> None:
        with tempfile.TemporaryDirectory() as outside_directory:
            with self.assertRaises(GitError):
                Repo.discover(Path(outside_directory))


if __name__ == "__main__":
    unittest.main()
