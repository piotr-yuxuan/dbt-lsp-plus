# SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
"""Tests of scripts/check-size, from the reduction chain's definition of its two figures."""

import importlib.machinery
import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check-size"

loader = importlib.machinery.SourceFileLoader("check_size", str(SCRIPT))
check_size = importlib.util.module_from_spec(importlib.util.spec_from_loader("check_size", loader))
loader.exec_module(check_size)


class Measure(unittest.TestCase):
    def test_counts_the_code_before_the_test_module_alone(self):
        text = "\n".join(
            [
                "// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later",
                "//! A module.",
                "/// A struct.",
                "",
                "use std::fmt;",
                "pub struct A;",
                "pub(crate) enum B {",
                "    One,",
                "}",
                "trait C {}",
                "type D = u8;",
                "#[cfg(test)]",
                "pub fn scratch() {}",
                "    #[cfg(test)]",
                "    mod inner {",
                "    }",
                "#[cfg(test)]",
                "mod tests {",
                "    struct Hidden;",
                "}",
            ]
        )
        self.assertEqual(check_size.measure(text), (12, 4))

    def test_counts_unions_and_unsafe_traits(self):
        text = "pub union U {\n    a: u8,\n}\nunsafe trait S {}\npub(crate) unsafe trait T {}\n"
        self.assertEqual(check_size.measure(text), (5, 3))

    def test_refuses_code_after_the_test_module(self):
        tests = ["pub fn a() {}", "#[cfg(test)]", "mod tests {", "    #[test]", "    fn t() {}", "}"]
        self.assertEqual(check_size.measure("\n".join([*tests, "", "// The end."])), (1, 0))
        for after in (["mod imp {", "    pub fn hidden() {}", "}"], ["pub struct Escaped;"]):
            with self.subTest(after=after):
                with self.assertRaisesRegex(ValueError, "after its test module"):
                    check_size.measure("\n".join([*tests, *after]))


class Main(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory(prefix="check-size-test.")
        self.addCleanup(scratch.cleanup)
        self.scratch = Path(scratch.name)

    def run_script(self, *arguments):
        return subprocess.run(
            [sys.executable, "-I", str(SCRIPT), *arguments],
            cwd=self.scratch,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_fails_only_above_a_ceiling(self):
        (self.scratch / "src" / "sub").mkdir(parents=True)
        (self.scratch / "src" / "a.rs").write_text("pub struct A;\nfn f() {}\n")
        (self.scratch / "src" / "sub" / "b.rs").write_text(
            "enum B {}\n\n// A comment.\n#[cfg(test)]\nmod tests {\n    struct T;\n}\n"
        )
        done = self.run_script("3", "2")
        self.assertEqual(done.returncode, 0)
        self.assertEqual(done.stdout, "check-size: 3 production lines, at most 3; 2 types, at most 2\n")
        for lines, types in (("2", "2"), ("3", "1")):
            with self.subTest(lines=lines, types=types):
                self.assertEqual(self.run_script(lines, types).returncode, 1)
        self.assertEqual(self.run_script("9", "9").returncode, 0)

    def test_names_a_file_with_code_after_its_test_module(self):
        (self.scratch / "src").mkdir()
        escaped = "#[cfg(test)]\nmod tests {\n}\npub struct Escaped;\n"
        (self.scratch / "src" / "a.rs").write_text(escaped)
        done = self.run_script("9", "9")
        self.assertEqual((done.returncode, done.stdout), (1, ""))
        self.assertIn("src/a.rs: code after its test module", done.stderr)

    def test_refuses_what_is_no_ceiling(self):
        (self.scratch / "src").mkdir()
        for arguments in (("x", "2"), ("3",), ("-1", "2")):
            with self.subTest(arguments=arguments):
                done = self.run_script(*arguments)
                self.assertEqual((done.returncode, done.stdout), (2, ""))
        (self.scratch / "src").rmdir()
        done = self.run_script("3", "2")
        self.assertEqual((done.returncode, done.stdout), (2, ""))


if __name__ == "__main__":
    unittest.main()
