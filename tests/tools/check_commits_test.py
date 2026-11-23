# SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
"""Tests of scripts/check-commits's size ratchet, from its header.

Each test makes its own repository in a scratch home, with the identity tests@example.invalid,
no global or system git configuration and an empty list of private words.  Every git command
must succeed, so that no case is checked on an empty range.
"""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check-commits"
RULES = "check-static: fmt size\n\nsize:\n\t@scripts/check-size $(SIZE_LINES) $(SIZE_TYPES)\n\n"


def makefile(lines=100, types=10, rules=RULES, extra=""):
    """A Makefile with the size rules RULES, the ceilings LINES and TYPES, then EXTRA."""
    return f"{rules}SIZE_LINES := {lines}\nSIZE_TYPES := {types}\n{extra}"


class Ceilings(unittest.TestCase):
    def setUp(self):
        scratch = Path(self.enterContext(tempfile.TemporaryDirectory(prefix="check-commits-test.")))
        self.repository = scratch / "repository"
        (scratch / "home").mkdir()
        (scratch / "private-words").write_text("")
        identity = {"NAME": "tests", "EMAIL": "tests@example.invalid"}
        self.env = {
            "PATH": os.environ["PATH"],
            "HOME": str(scratch / "home"),
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "DBT_LSP_PLUS_PRIVATE_WORDS": str(scratch / "private-words"),
            "DBT_LSP_PLUS_CHECK_USER": "x",
        }
        for who in ("AUTHOR", "COMMITTER"):
            self.env.update({f"GIT_{who}_{key}": value for key, value in identity.items()})
        self.repository.mkdir()
        self.git("init", "-q", "-b", "main")
        self.commit("Makefile", makefile(), "Start")
        self.git("tag", "base")

    def git(self, *arguments):
        here = {"cwd": self.repository, "env": self.env}
        subprocess.run(["git", *arguments], **here, capture_output=True, check=True)

    def check(self, base="base"):
        here = {"cwd": self.repository, "env": self.env}
        return subprocess.run([str(SCRIPT), base], **here, capture_output=True, text=True, check=False)

    def branch(self, name, start="base"):
        self.git("checkout", "-q", "-b", name, start)

    def commit(self, name, text, message):
        (self.repository / name).write_text(text)
        self.git("add", name)
        self.git("commit", "-q", "-m", message)

    def merge(self, other, text, message):
        """Merge OTHER into the branch, its Makefile resolved as TEXT."""
        self.git("merge", "-q", "--no-ff", "--no-commit", other)
        self.commit("Makefile", text, message)

    def refused(self, cases):
        """Each case, a Makefile, a message and what the finding names, on a branch of its own."""
        for at, (text, message, named) in enumerate(cases):
            with self.subTest(message=message, named=named):
                self.branch(f"case-{at}")
                self.commit("Makefile", text, message)
                done = self.check()
                self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
                self.assertIn(named, done.stdout)

    def test_a_raise_of_a_ceiling_needs_its_word_in_the_body(self):
        empty = "Raise the lines\n\nRaises the size ceilings:"
        self.refused(
            [
                (makefile(lines=101), "Raise the lines", "SIZE_LINES raised from 100 to 101"),
                (makefile(types=11), "Raise the types\n\nFor room.", "SIZE_TYPES raised from 10"),
                (RULES + "SIZE_TYPES := 10\n", "Drop the lines", "SIZE_LINES is not one line"),
                (makefile(lines=101), empty, "SIZE_LINES raised"),
                (makefile(lines=101), "Raises the size ceilings: the MCP server", "SIZE_LINES raised"),
            ]
        )
        self.branch("said")
        why = "Raises the size ceilings: the MCP server, as the roadmap allows"
        self.commit("Makefile", makefile(lines=101), f"Raise the lines\n\n{why}")
        done = self.check()
        self.assertEqual((done.returncode, done.stdout), (0, ""), done.stderr)

    def test_another_assignment_of_a_ceiling_needs_its_word(self):
        rules = "the size check's rules changed"
        self.refused(
            [
                (makefile(extra="SIZE_LINES := 99999\n"), "Add a line", "SIZE_LINES is not one line"),
                (makefile(extra="SIZE_TYPES = 999\n"), "Add a recursive one", rules),
                (makefile(extra="SIZE_LINES ?= 50\n"), "Add a conditional one", rules),
                (makefile(extra="SIZE_LINES += 1\n"), "Add to the lines", rules),
                (makefile(extra="SIZE_LINES:=99999\n"), "Add one without spaces", rules),
                (makefile(extra="override SIZE_LINES := 50\n"), "Override the lines", rules),
            ]
        )

    def test_a_change_to_how_the_size_is_checked_needs_its_word(self):
        rules = "the size check's rules changed"
        recipe = RULES.replace("$(SIZE_LINES)", "9999")
        self.refused(
            [
                (makefile(rules=recipe), "Loosen the recipe", rules),
                (makefile(rules=RULES.replace(" size\n", "\n", 1)), "Drop the size check", rules),
                (makefile(rules=RULES.replace("size:", "size-off:")), "Rename the size check", rules),
            ]
        )

    def test_a_merge_is_held_to_its_first_parent(self):
        self.branch("side")
        self.commit("README", "text\n", "Add a file")
        self.branch("raised")
        self.merge("side", makefile(lines=101), "Merge side")
        done = self.check()
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("SIZE_LINES raised from 100 to 101", done.stdout)
        self.branch("lowered")
        self.merge("side", makefile(lines=99), "Merge side")
        done = self.check()
        self.assertEqual((done.returncode, done.stdout), (0, ""), done.stderr)

    def test_a_ceiling_lowered_introduced_or_untouched_needs_no_word(self):
        self.branch("lowered")
        self.commit("Makefile", makefile(lines=99, types=9), "Lower the ceilings")
        self.branch("introduced")
        self.commit("Makefile", "check:\n", "Drop the ceilings")
        self.git("tag", "none")
        self.commit("Makefile", makefile(), "Add the ceilings")
        self.branch("untouched")
        self.commit("README", "text\n", "Add a file")
        self.branch("elsewhere")
        extra = "PYTHON := scripts/check-size tests\n\nother:\n\t@true\n"
        self.commit("Makefile", makefile(extra=extra), "Add a target")
        bases = {"lowered": "base", "introduced": "none", "untouched": "base", "elsewhere": "base"}
        for branch, base in bases.items():
            with self.subTest(branch=branch):
                self.git("checkout", "-q", branch)
                done = self.check(base)
                self.assertEqual((done.returncode, done.stdout), (0, ""), done.stderr)


if __name__ == "__main__":
    unittest.main()
