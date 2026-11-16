# SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
"""Tests of scripts/fetch-dbt, from its header, on local archives through file:// URLs: no network."""

import contextlib
import hashlib
import http.client
import importlib.machinery
import importlib.util
import io
import json
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

    def test_the_hash_is_checked_before_the_archive_is_read(self):
        path = self.scratch / "not-a-tar"
        path.write_bytes(b"not a tar at all")
        with self.assertRaisesRegex(fetch_dbt.FetchError, "SHA-256"):
            fetch_dbt.fetch(path.as_uri(), "0" * 64, self.out)
        self.assertEqual(list(self.out.iterdir()), [])

    def test_a_failure_leaves_no_older_binary_either(self):
        url, _ = self.archive({"dbt": b"binary"})
        self.out.mkdir()
        (self.out / "dbt").write_bytes(b"older build")
        with self.assertRaises(fetch_dbt.FetchError):
            fetch_dbt.fetch(url, "0" * 64, self.out)
        self.assertEqual(list(self.out.iterdir()), [])

    def test_only_a_regular_file_named_dbt_counts(self):
        path = self.scratch / "archive-0.tar.gz"
        with tarfile.open(path, "w:gz") as tar:
            directory = tarfile.TarInfo("dbt")
            directory.type = tarfile.DIRTYPE
            tar.addfile(directory)
            link = tarfile.TarInfo("link/dbt")
            link.type = tarfile.SYMTYPE
            link.linkname = "/etc/passwd"
            tar.addfile(link)
            info = tarfile.TarInfo("bin/dbt")
            info.size = 6
            tar.addfile(info, io.BytesIO(b"binary"))
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        self.assertEqual(fetch_dbt.fetch(path.as_uri(), digest, self.out).read_bytes(), b"binary")

    def test_a_protocol_error_gives_status_2_and_no_traceback(self):
        def broken(*_):
            raise http.client.BadStatusLine("garbage")

        before = fetch_dbt.fetch
        fetch_dbt.fetch = broken
        self.addCleanup(setattr, fetch_dbt, "fetch", before)
        self.assertEqual(fetch_dbt.main(["oss", str(self.out)]), 2)

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
        for arguments in (
            [],
            ["oss"],
            ["nightly", "dir"],
            ["oss", "dir", "more"],
            ["--latest"],
            ["--latest", "oss"],
        ):
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
                self.assertRegex(
                    done.stderr, re.escape("usage: scripts/fetch-dbt [--latest] oss|proprietary DIR")
                )
                self.assertEqual(list(self.scratch.iterdir()), [])


def asset(name):
    """An asset as GitHub lists it, with a digest of its own."""
    return {"name": name, "digest": "sha256:" + hashlib.sha256(name.encode()).hexdigest()}


def release(tag, draft=False, prerelease=False, assets=None):
    """A release as GitHub's API lists it, with the archives of each target unless ASSETS."""
    version = tag.removeprefix("v")
    targets = [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
    ]
    archives = [asset(f"dbt-core-{version}-{target}.tar.gz") for target in targets]
    listed = archives + [asset("SHA256SUMS")] if assets is None else assets
    return {"tag_name": tag, "draft": draft, "prerelease": prerelease, "assets": listed}


def digest_of(version):
    return hashlib.sha256(f"dbt-core-{version}-x86_64-unknown-linux-gnu.tar.gz".encode()).hexdigest()


class Latest(unittest.TestCase):
    """The latest builds, from listings in the shapes that GitHub and dbt Labs gave on 2026-11-16."""

    def test_the_highest_stable_release_by_number_that_holds_its_archive_and_digest(self):
        # Newest first, as GitHub lists them, the highest neither the first nor the last valid.
        releases = [
            release("v2.0.30", assets=[asset("dbt-core-2.0.30-aarch64-apple-darwin.tar.gz")]),
            release("v2.0.13", prerelease=True),
            release("v2.0.12", draft=True),
            release("v2.0.0-dev.44-test-pypi", prerelease=True),
            release("v2.0.8"),
            release("v2.0.11"),
            release("v2.0.2-test-pypi", prerelease=True),
            release("v1.12.7", assets=[asset("dbt_core-1.12.7-py3-none-any.whl")]),
            "not a release",
            release("v2.0.3"),
        ]
        version, url, digest = fetch_dbt.latest_oss(releases)
        self.assertEqual(version, "2.0.11")
        self.assertEqual(
            url,
            "https://github.com/dbt-labs/dbt/releases/download/v2.0.11/dbt-core-2.0.11-x86_64-unknown-linux-gnu.tar.gz",
        )
        self.assertEqual(digest, digest_of("2.0.11"))

    def test_a_release_whose_archive_has_no_usable_digest_is_an_error(self):
        name = "dbt-core-2.0.12-x86_64-unknown-linux-gnu.tar.gz"
        for archives in (
            [{"name": name}],
            [{"name": name, "digest": None}],
            [{"name": name, "digest": "sha512:" + "0" * 128}],
            [asset(name), asset(name)],
        ):
            with self.subTest(archives=archives), self.assertRaises(fetch_dbt.FetchError):
                fetch_dbt.latest_oss([release("v2.0.12", assets=archives), release("v2.0.11")])

    def test_no_release_with_its_archive_is_an_error(self):
        for releases in (
            [],
            [release("v1.12.7", assets=[])],
            [release("v2.0.13", prerelease=True)],
            {"message": "API rate limit"},
        ):
            with self.subTest(releases=releases), self.assertRaises(fetch_dbt.FetchError):
                fetch_dbt.latest_oss(releases)

    def test_the_stable_proprietary_build(self):
        versions = {
            "stable": {"tag": "v2.0.11"},
            "canary": {"tag": "v2.0.12"},
            "latest": {"tag": "v2.0.12"},
        }
        version, url = fetch_dbt.latest_proprietary(versions)
        self.assertEqual(version, "2.0.11")
        self.assertEqual(
            url, "https://public.cdn.getdbt.com/fs/cli/fs-v2.0.11-x86_64-unknown-linux-gnu.tar.gz"
        )
        for versions in ({"stable": {"tag": "2.0.11"}}, {"stable": "v2.0.11"}, {}, []):
            with self.subTest(versions=versions), self.assertRaises(fetch_dbt.FetchError):
                fetch_dbt.latest_proprietary(versions)

    def test_the_sidecar_must_name_its_archive(self):
        url = "https://public.cdn.getdbt.com/fs/cli/fs-v2.0.11-x86_64-unknown-linux-gnu.tar.gz"
        digest = hashlib.sha256(b"archive").hexdigest()
        self.assertEqual(
            fetch_dbt.sidecar_digest(f"{digest}  fs-v2.0.11-x86_64-unknown-linux-gnu.tar.gz\n", url),
            digest,
        )
        for text in (
            f"{digest}  fs-v2.0.12-x86_64-unknown-linux-gnu.tar.gz",
            f"{digest[:-1]}  fs-v2.0.11-x86_64-unknown-linux-gnu.tar.gz",
            f"{digest.upper()}  fs-v2.0.11-x86_64-unknown-linux-gnu.tar.gz",
            f"{digest}  fs-v2.0.11-x86_64-unknown-linux-gnu.tar.gz extra",
            "<html>Not Found</html>",
        ):
            with self.subTest(text=text), self.assertRaises(fetch_dbt.FetchError):
                fetch_dbt.sidecar_digest(text, url)


class Finders(unittest.TestCase):
    """What the finders ask of GitHub and dbt Labs, with `get` answering recorded bodies."""

    def answering(self, bodies):
        asked = []

        def get(url, headers=None):
            asked.append((url, headers or {}))
            return bodies[url]

        before = fetch_dbt.get
        fetch_dbt.get = get
        self.addCleanup(setattr, fetch_dbt, "get", before)
        return asked

    def test_github_s_listing_is_asked_with_the_token(self):
        asked = self.answering({fetch_dbt.RELEASES: json.dumps([release("v2.0.11")]).encode()})
        before = os.environ.get("GH_TOKEN")
        os.environ["GH_TOKEN"] = "the-token"
        self.addCleanup(
            lambda: os.environ.pop("GH_TOKEN") if before is None else os.environ.update(GH_TOKEN=before)
        )
        self.assertEqual(fetch_dbt.find_oss()[0], "2.0.11")
        self.assertEqual(asked[0][0], "https://api.github.com/repos/dbt-labs/dbt/releases?per_page=100")
        self.assertEqual(asked[0][1].get("Authorization"), "Bearer the-token")

    def test_the_proprietary_digest_is_read_beside_its_archive(self):
        url = "https://public.cdn.getdbt.com/fs/cli/fs-v2.0.11-x86_64-unknown-linux-gnu.tar.gz"
        digest = hashlib.sha256(b"archive").hexdigest()
        asked = self.answering(
            {
                "https://public.cdn.getdbt.com/fs/versions.json": b'{"stable": {"tag": "v2.0.11"}}',
                f"{url}.sha256": f"{digest}  fs-v2.0.11-x86_64-unknown-linux-gnu.tar.gz\n".encode(),
            }
        )
        self.assertEqual(fetch_dbt.find_proprietary(), ("2.0.11", url, digest))
        self.assertEqual(
            [url for url, _ in asked],
            ["https://public.cdn.getdbt.com/fs/versions.json", f"{url}.sha256"],
        )


class LatestCommand(unittest.TestCase):
    """`--latest` from its command line: the line it prints, and what a failure leaves."""

    def setUp(self):
        scratch = tempfile.TemporaryDirectory(prefix="fetch-dbt-test.")
        self.addCleanup(scratch.cleanup)
        self.scratch = Path(scratch.name)
        self.out = self.scratch / "out"
        self.archive = self.scratch / "a.tar.gz"
        with tarfile.open(self.archive, "w:gz") as tar:
            info = tarfile.TarInfo("dbt")
            info.size = 6
            tar.addfile(info, io.BytesIO(b"binary"))
        before = fetch_dbt.LATEST["oss"]
        self.addCleanup(fetch_dbt.LATEST.__setitem__, "oss", before)

    def run_latest(self, finder):
        fetch_dbt.LATEST["oss"] = finder
        printed, complained = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(printed), contextlib.redirect_stderr(complained):
            status = fetch_dbt.main(["--latest", "oss", str(self.out)])
        return status, printed.getvalue().splitlines(), complained.getvalue().splitlines()

    def test_prints_what_it_found_then_fetches_it(self):
        digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        status, printed, _ = self.run_latest(lambda: ("9.9.9", self.archive.as_uri(), digest))
        self.assertEqual(status, 0)
        self.assertEqual(
            printed, [f"oss 9.9.9 {self.archive.as_uri()} {digest}", str(self.out / "dbt")]
        )
        self.assertEqual((self.out / "dbt").read_bytes(), b"binary")

    def test_the_line_is_printed_even_when_the_archive_is_not_what_it_found(self):
        status, printed, complained = self.run_latest(
            lambda: ("9.9.9", self.archive.as_uri(), "0" * 64)
        )
        self.assertEqual((status, printed), (2, [f"oss 9.9.9 {self.archive.as_uri()} {'0' * 64}"]))
        self.assertEqual(len(complained), 1)
        self.assertFalse((self.out / "dbt").exists())

    def test_a_failed_lookup_leaves_no_older_binary(self):
        def failing(error):
            def finder():
                raise error

            return finder

        for error in (fetch_dbt.FetchError("no release"), ValueError("not JSON")):
            with self.subTest(error=error):
                self.out.mkdir(exist_ok=True)
                (self.out / "dbt").write_bytes(b"older build")
                status, printed, complained = self.run_latest(failing(error))
                self.assertEqual((status, printed, len(complained)), (2, [], 1))
                self.assertFalse((self.out / "dbt").exists())

    def test_a_listing_nested_too_deep_is_status_2(self):
        before = fetch_dbt.get
        fetch_dbt.get = lambda *_: b"[" * 200000
        self.addCleanup(setattr, fetch_dbt, "get", before)
        status, _, complained = self.run_latest(fetch_dbt.find_oss)
        self.assertEqual((status, len(complained)), (2, 1))


if __name__ == "__main__":
    unittest.main()
