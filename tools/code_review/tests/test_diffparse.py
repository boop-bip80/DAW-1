"""Tests for the unified-diff parser."""

from __future__ import annotations

import unittest

from tools.code_review.diffparse import parse, unquote_path

DIFF = """diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 fn main() {
-    old();
+    new();
+    added();
 }
diff --git a/new.rs b/new.rs
new file mode 100644
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,2 @@
+fn a() {}
+fn b() {}
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
--- a/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-fn gone() {}
"""


class ParseTests(unittest.TestCase):
    def test_status_and_paths(self) -> None:
        files = parse(DIFF)
        self.assertEqual([file.path for file in files], ["src/lib.rs", "new.rs", "gone.rs"])
        self.assertEqual([file.status for file in files], ["modified", "added", "deleted"])

    def test_added_line_numbers(self) -> None:
        files = parse(DIFF)
        self.assertEqual(files[0].added_line_numbers(), {2, 3})
        self.assertEqual(files[0].added_line_text()[2], "    new();")
        self.assertEqual(files[0].added, 2)
        self.assertEqual(files[0].removed, 1)

    def test_new_file_hunk(self) -> None:
        files = parse(DIFF)
        self.assertEqual(files[1].added_line_numbers(), {1, 2})
        self.assertEqual(files[1].hunks[0].new_start, 1)

    def test_quoted_path(self) -> None:
        self.assertEqual(unquote_path('"dir with space/file.rs"'), "dir with space/file.rs")
        self.assertEqual(unquote_path('"caf\\303\\251.rs"'), "café.rs")

    def test_binary_file_has_no_hunks(self) -> None:
        files = parse(
            "diff --git a/icon.png b/icon.png\n"
            "new file mode 100644\n"
            "Binary files /dev/null and b/icon.png differ\n"
        )
        self.assertEqual(len(files), 1)
        self.assertTrue(files[0].binary)
        self.assertEqual(files[0].added_line_numbers(), set())


if __name__ == "__main__":
    unittest.main()
