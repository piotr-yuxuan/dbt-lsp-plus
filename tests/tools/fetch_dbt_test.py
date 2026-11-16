# SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
"""Tests of scripts/fetch-dbt, from its header, on local archives through file:// URLs: no network."""

import hashlib
import importlib.machinery
import importlib.util
import io
import os
import re
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "fetch-dbt"

loader = importlib.machinery.SourceFileLoader("fetch_dbt", str(SCRIPT))
fetch_dbt = importlib.util.module_from_spec(importlib.util.spec_from_loader("fetch_dbt", loader))
loader.exec_module(fetch_dbt)


class Fetch(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory(prefix="fetch-dbt-test.")
        self.addCleanup(scratch.cleanup)
        self.scratch = Path(scratch.name)
        self.out = self.scratch / "out"

    def archive(self, members):
        """A gzipped tar of MEMBERS, file names to bytes, and its SHA-256."""
        path = self.scratch / f"archive-{len(list(self.scratch.glob('archive-*')))}.tar.gz"
        with tarfile.open(path, "w:gz") as tar:
            for name, data in members.items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
        return path.as_uri(), hashlib.sha256(path.read_bytes()).hexdigest()

    def test_keeps_the_binary_alone_whichever_directory_holds_it(self):
        for name in ("dbt", "dbt-core-2.0.11/dbt"):
            with self.subTest(name=name):
                self.setUp()
                url, digest = self.archive({name: b"binary", "LICENSE": b"text"})
                binary = fetch_dbt.fetch(url, digest, self.out)
                self.assertEqual(binary, self.out / "dbt")
                self.assertEqual(binary.read_bytes(), b"binary")
                self.assertTrue(os.access(binary, os.X_OK))
                self.assertEqual([path.name for path in self.out.iterdir()], ["dbt"])

    def test_a_wrong_hash_leaves_no_binary(self):
        url, _ = self.archive({"dbt": b"binary"})
        with self.assertRaisesRegex(fetch_dbt.FetchError, "SHA-256"):
            fetch_dbt.fetch(url, "0" * 64, self.out)
        self.assertEqual(list(self.out.iterdir()), [])

    def test_an_archive_without_one_dbt_leaves_no_binary(self):
        for members in ({"bin/other": b""}, {"a/dbt": b"1", "b/dbt": b"2"}):
            with self.subTest(members=members):
                self.setUp()
                url, digest = self.archive(members)
                with self.assertRaisesRegex(fetch_dbt.FetchError, "named dbt"):
                    fetch_dbt.fetch(url, digest, self.out)
                self.assertEqual(list(self.out.iterdir()), [])

    def test_no_path_of_the_archive_is_written(self):
        url, digest = self.archive({"../../dbt": b"binary"})
        fetch_dbt.fetch(url, digest, self.out)
        self.assertEqual((self.out / "dbt").read_bytes(), b"binary")
        self.assertFalse((self.scratch.parent / "dbt").exists())
        written = sorted(path.name for path in self.scratch.iterdir())
        self.assertEqual(written, ["archive-0.tar.gz", "out"])

    def test_a_download_past_its_size_stops_and_leaves_no_binary(self):
        url, digest = self.archive({"dbt": b"binary" * 1000})
        before = fetch_dbt.LIMIT_BYTES
        fetch_dbt.LIMIT_BYTES = 100
        self.addCleanup(setattr, fetch_dbt, "LIMIT_BYTES", before)
        with self.assertRaisesRegex(fetch_dbt.FetchError, "more than 100 bytes"):
            fetch_dbt.fetch(url, digest, self.out)
        self.assertEqual(list(self.out.iterdir()), [])

    def test_each_pin_is_an_https_url_and_a_sha256(self):
        self.assertEqual(sorted(fetch_dbt.BUILDS), ["oss", "proprietary"])
        for build, (url, digest) in fetch_dbt.BUILDS.items():
            with self.subTest(build=build):
                self.assertTrue(url.startswith("https://"))
                self.assertRegex(digest, r"\A[0-9a-f]{64}\Z")

    def test_wrong_arguments_give_the_usage_and_status_2(self):
        for arguments in ([], ["oss"], ["nightly", "dir"], ["oss", "dir", "more"]):
            with self.subTest(arguments=arguments):
                done = subprocess.run(
                    ["python3", str(SCRIPT), *arguments],
                    cwd=self.scratch,
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )
                self.assertEqual(done.returncode, 2)
                self.assertRegex(done.stderr, re.escape("usage: scripts/fetch-dbt oss|proprietary DIR"))
                self.assertEqual(list(self.scratch.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
