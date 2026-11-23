# SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
"""Times dbt-lsp-plus on a project that generate.py made.

usage: measure.py PROXY FAKE_DBT ROOT RUNS

Starts PROXY with FAKE_DBT as its dbt (the tests' fake-dbt-lsp, whose
parse writes no manifest) on the project ROOT.
It times each request RUNS times, warm: median and minimum in milliseconds,
from the request written to its answer read.  Then, for each file of SAVED
and each request of the budget, it changes the file on disk, tells the
proxy it was saved, and times the request once, the first after the save,
which pays what the save costs.  The first request of a session is timed apart,
right after `initialize` and a few seconds later, as an editor's first
request comes once the user has done something.  The project's files are
changed: generate it again before another run.
"""

import json
import os
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# The seconds an editor's first request comes after `initialize`.
FIRST_AFTER = 3

# The requests of plan 016's budget, by the names of `requests`.
BUDGET = [
    "workspace symbol",
    "definition of a ref in YAML",
    "references of a source",
    "references of a model",
    "references of a macro",
    "rename of a source",
    "prepare hierarchy",
    "incoming calls",
]


class Client:
    """A client of one proxy process, over its standard input and output."""

    def __init__(self, command: list[str], env: dict[str, str]) -> None:
        """Start COMMAND in ENV."""
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env)
        self.next_id = 0

    def send(self, message: dict) -> None:
        """Write MESSAGE as one frame."""
        body = json.dumps(message).encode()
        self.process.stdin.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
        self.process.stdin.flush()

    def read(self) -> dict:
        """Read one frame."""
        length = 0
        while (line := self.process.stdout.readline()) not in (b"\r\n", b""):
            if line.lower().startswith(b"content-length:"):
                length = int(line.split(b":")[1])
        return json.loads(self.process.stdout.read(length))

    def request(self, method: str, params: dict) -> tuple[float, dict]:
        """Send request METHOD; the milliseconds its answer took, and the answer."""
        self.next_id += 1
        start = time.perf_counter()
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        while (message := self.read()).get("id") != self.next_id or "method" in message:
            pass
        return (time.perf_counter() - start) * 1000, message

    def notify(self, method: str, params: dict) -> None:
        """Send notification METHOD."""
        self.send({"jsonrpc": "2.0", "method": method, "params": params})


def at(path: Path, needle: str, delta: int) -> dict:
    """The parameters of a request DELTA characters into the first NEEDLE of PATH."""
    text = path.read_text()
    offset = text.index(needle) + delta
    line, start = text.count("\n", 0, offset), text.rfind("\n", 0, offset) + 1
    position = {"line": line, "character": offset - start}
    return {"textDocument": {"uri": path.as_uri()}, "position": position}


def requests(root: Path) -> dict[str, tuple[str, dict]]:
    """The requests to time on the generated project at ROOT, by name."""
    staging = root / "models/staging/group_0/sta_0.sql"
    marts = sorted(root.glob("models/marts/group_*"))
    middle = marts[len(marts) // 2]
    mart, properties = min(middle.glob("*.sql")), middle / "_models.yml"
    source = at(staging, "source('", 9)
    declaration = {"context": {"includeDeclaration": True}}
    found = {
        "workspace symbol": ("workspace/symbol", {"query": "int_15"}),
        "definition of a ref in YAML": ("textDocument/definition", at(properties, "ref('", 6)),
        "definition of a source in SQL": ("textDocument/definition", source),
        "references of a source": ("textDocument/references", {**source, **declaration}),
        "references of a model": ("textDocument/references", {**at(mart, "ref('", 6), **declaration}),
        "references of a macro": ("textDocument/references", {**at(staging, "m_0(", 1), **declaration}),
        "rename of a source": ("textDocument/rename", {**source, "newName": "renamed"}),
        "prepare hierarchy": ("textDocument/prepareCallHierarchy", at(mart, "ref('", 6)),
        "incoming calls": (
            "callHierarchy/incomingCalls",
            {"item": {"data": {"uniqueId": "model.synth.sta_0"}}},
        ),
        "hover on a doc in YAML": ("textDocument/hover", at(properties, 'doc("', 6)),
    }
    big = root / "models/big/_big.yml"
    if big.exists():
        found["definition in the big YAML"] = ("textDocument/definition", at(big, "ref('", 6))
    return found


def saved(root: Path) -> list[Path]:
    """The files saved after the warm measures: a model, the largest properties file, the project's."""
    marts = sorted(root.glob("models/marts/group_*/*.sql"))
    largest = max(root.glob("models/**/*.yml"), key=lambda path: path.stat().st_size)
    return [marts[0], largest, root / "dbt_project.yml"]


def start(proxy: str, fake: str, root: Path, scratch: Path) -> Client:
    """PROXY started with FAKE as its dbt on ROOT, initialized."""
    env = {
        "PATH": os.environ["PATH"],
        "HOME": str(scratch),
        "XDG_CACHE_HOME": str(scratch / "cache"),
    }
    client = Client([proxy, fake, "lsp", "--project-dir", str(root)], env)
    capabilities = {"textDocument": {"documentSymbol": {"hierarchicalDocumentSymbolSupport": True}}}
    initialize = {"processId": None, "rootUri": root.as_uri(), "capabilities": capabilities}
    took, _ = client.request("initialize", initialize)
    client.notify("initialized", {})
    sys.stdout.write(f"{'initialize':52} once   {took:8.1f} ms\n")
    return client


def first(proxy: str, fake: str, root: Path, pause: float) -> None:
    """Time a session's first request, a workspace symbol, PAUSE seconds after `initialize`."""
    with tempfile.TemporaryDirectory() as scratch:
        client = start(proxy, fake, root, Path(scratch))
        time.sleep(pause)
        took, _ = client.request(*requests(root)["workspace symbol"])
        label = f"first request, {pause} s after initialize"
        sys.stdout.write(f"{label:52} once   {took:8.1f} ms\n")
        client.process.stdin.close()
        client.process.wait()


def main() -> None:
    """Time each request warm, then each of the budget's first after a save, one line each."""
    proxy, fake, root, runs = sys.argv[1], sys.argv[2], Path(sys.argv[3]).resolve(), int(sys.argv[4])
    every = requests(root)
    for pause in (0, FIRST_AFTER):
        first(proxy, fake, root, pause)
    with tempfile.TemporaryDirectory() as scratch:
        client = start(proxy, fake, root, Path(scratch))
        client.request(*every["workspace symbol"])
        for name, (method, params) in every.items():
            times = [client.request(method, params)[0] for _ in range(runs)]
            median = statistics.median(times)
            sys.stdout.write(f"{name:52} median {median:8.1f} ms  min {min(times):8.1f} ms\n")
        for path in saved(root):
            comment = "#" if path.suffix == ".yml" else "--"
            for name in BUDGET:
                path.write_text(path.read_text() + f"{comment} saved\n")
                client.notify("textDocument/didSave", {"textDocument": {"uri": path.as_uri()}})
                took, _ = client.request(*every[name])
                label = f"{name}, {path.name} saved"
                sys.stdout.write(f"{label:52} once   {took:8.1f} ms\n")
        client.process.stdin.close()
        client.process.wait()


main()
