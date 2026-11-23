# SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
"""Tests of scripts/check-commits's rule on the size ceilings, from its header.

Each test makes its own repository in a scratch home, with the identity tests@example.invalid,
no global or system git configuration and an empty list of private words.
"""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check-commits"
CEILINGS = "SIZE_LINES := 100\nSIZE_TYPES := 10\n"


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
        self.run_in_repository("git", "init", "-q", "-b", "main")
        self.commit("Makefile", "check:\n" + CEILINGS, "Start")
        self.run_in_repository("git", "tag", "base")

    def run_in_repository(self, *command):
        return subprocess.run(
            command, cwd=self.repository, env=self.env, capture_output=True, text=True, check=False
        )

    def branch(self, name):
        self.run_in_repository("git", "checkout", "-q", "-b", name, "base")

    def commit(self, name, text, message):
        (self.repository / name).write_text(text)
        self.run_in_repository("git", "add", name)
        self.run_in_repository("git", "commit", "-q", "-m", message)

    def test_a_raise_of_a_ceiling_needs_its_word_in_the_body(self):
        raised = "check:\nSIZE_LINES := 101\nSIZE_TYPES := 10\n"
        types = "check:\nSIZE_LINES := 100\nSIZE_TYPES := 11\n"
        cases = [
            (raised, "Raise the lines", "SIZE_LINES"),
            (types, "Raise the types\n\nFor room.", "SIZE_TYPES"),
            ("check:\nSIZE_TYPES := 10\n", "Drop the lines", "SIZE_LINES"),
            (raised, "Raise the lines\n\nRaises the size ceilings:", "SIZE_LINES"),
            (raised, "Raises the size ceilings: the MCP server", "SIZE_LINES"),
        ]
        for at, (makefile, message, name) in enumerate(cases):
            with self.subTest(message=message):
                self.branch(f"case-{at}")
                self.commit("Makefile", makefile, message)
                done = self.run_in_repository(str(SCRIPT), "base")
                self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
                self.assertIn(name, done.stdout)
        self.branch("said")
        why = "Raises the size ceilings: the MCP server, as the roadmap allows"
        self.commit("Makefile", raised, f"Raise the lines\n\n{why}")
        done = self.run_in_repository(str(SCRIPT), "base")
        self.assertEqual((done.returncode, done.stdout), (0, ""), done.stderr)

    def test_a_ceiling_lowered_introduced_or_untouched_needs_no_word(self):
        self.branch("lowered")
        self.commit("Makefile", "check:\nSIZE_LINES := 99\nSIZE_TYPES := 10\n", "Lower the lines")
        self.branch("introduced")
        self.commit("Makefile", "check:\n", "Drop the ceilings")
        self.run_in_repository("git", "tag", "none")
        self.commit("Makefile", "check:\n" + CEILINGS, "Add the ceilings")
        self.branch("untouched")
        self.commit("README", "text\n", "Add a file")
        for branch, base in (("lowered", "base"), ("introduced", "none"), ("untouched", "base")):
            with self.subTest(branch=branch):
                self.run_in_repository("git", "checkout", "-q", branch)
                done = self.run_in_repository(str(SCRIPT), base)
                self.assertEqual((done.returncode, done.stdout), (0, ""), done.stderr)


if __name__ == "__main__":
    unittest.main()
