# Design of dbt-lsp-plus

dbt-lsp-plus is a language server for dbt projects.  It answers from the
project's files, which it scans, and from the user's own `dbt parse`,
which it runs with whichever dbt 2 the server command names: dbt-oss
(observed on 2.0.11) or dbt Labs' build (observed on 2.0.9), whose `dbt
lsp` it never runs.  An editor gets its features through standard LSP,
and so does an agent whose LSP tool speaks definition, references, hover,
document and workspace symbols and the call hierarchy.  Until plan 075 it
ran `dbt lsp` and forwarded to it what it did not answer (see
"History").

## Boundary

- It uses the user's own dbt CLI, whose `dbt parse` gives the resolved
  configuration that the files alone do not.
- It unlocks nothing that dbt gates behind an account, and has no
  column-level features.
- It runs the `dbt` the user installed, and ships no part of it.
- Standard LSP is its only contract with an editor.  dbt-mode needs nothing
  specific to it: dbt-mode steps aside, feature by feature, for each
  capability the server announces, and keeps working without it.

## Language: Rust

- One static binary that any editor or agent starts as a stdio server, with
  no runtime to install beside dbt.
- refmux (`circonomit/refmux`, Apache-2.0), the prior art, is Rust.  It runs
  one `dbt lsp` per project of a monorepo and adds no features.
- Python with pygls was set aside while dbt-lsp-plus was a proxy, which
  had to pass unknown messages byte for byte.  Go would serve as well as
  Rust; Rust won on refmux.

## Message flow

```
client ── stdin ──▶ dbt-lsp-plus ── runs ──▶ dbt parse, dbt compile, dbt list
       ◀─ stdout ──
```

The crate `lsp-server` (plan 083) reads and writes the framing on
threads of its own; one thread hands the client's messages to one event
loop, which owns all state; the parse, the runs of dbt's CLI and the
watchers of the last run report to it from threads of their own.

- `initialize` goes through `lsp-server`'s handshake (plan 083).  Before
  it, a request gets -32002 and a notification is dropped; `exit` ends
  the server, status 1.  Its answer, computed at once, is sent, then the
  crate waits for `initialized`: any other message ends the session,
  status 1.  The first scan, the watcher of runs, the watcher of files
  and the line that names the project start once `initialized` comes.  A
  second `initialize` gets -32601, as a name with no row.  The answer
  holds an object written beside `CAPABILITIES` (see "Capabilities"): a
  provider for each method but the call hierarchy's followers, saves,
  which start a parse, and the commands `dbt-lsp-plus.nodeInfo`, `dbt-lsp-plus.compile` and
  `dbt-lsp-plus.nodes`.  `initializationOptions` is not read.  Positions
  are in UTF-16, LSP's default, which the server keeps by announcing no
  `positionEncoding`.
- A request names a row of `CAPABILITIES` (`src/capability.rs`), a
  command only through `workspace/executeCommand`; with none, it gets
  error -32601 `dbt-lsp-plus does not answer <name>`, before a project
  too, `shutdown` aside, null as LSP asks: no one else will answer it
  (plan 075).  Before a project, a row's request gets null (see "Root");
  after, its handler's answer, at any position of any file: its nothing,
  null or `[]` for a list, where the position names nothing.  References
  come from the scan whichever file asks, while a source's rename needs
  a file the scan holds, since it would leave the asking file as it is.
- After `shutdown`, every request gets -32600, as LSP says.
- Request ids: the client's, a string or a 32-bit integer, pass
  unchanged; a request of any other id (above 2^31 - 1, fractional,
  null) is read as a notification and never answered.  The server's own requests to
  the client (`workspace/inlayHint/refresh`, `client/registerCapability`)
  carry string ids `dbt-lsp-plus/N`; the client's answers to them are
  dropped.
- A panic while the server answers a request fails that request with
  -32603; the session goes on.  No input is known to cause one, so no
  test does.

### What a request gets

Every request the server answers gets one value, the Answer
(`src/workspace.rs`, plan 044; `proxy::frame` renders it): found, a result, an empty one included;
empty, the method's nothing, null or `[]` for a list, as its capability
says, where the request's position names nothing to look up; or
unanswered, with its reasons.  A target that resolves to nothing is
found, a definition's null or references' `[]`; a position that names no
target is empty, for a definition, references and the call hierarchy
alike; nodeInfo by an id that names no node is empty.  LSP renders found
and empty alike; a rendering that tells them apart (plan 052) reads only
empty as nothing to look up.  An unanswered request is error -32803
whose message is its reasons, joined by `; `, as nodeInfo's has been
since plan 038.  The protocol's own errors (-32600, -32601, -32602,
-32603) say that the request is wrong or unknown, or that the server
broke.  The reasons are one closed set, whose words live in one place,
`Reason`'s `Display`:

| Reason | Message |
|---|---|
| `Unreadable` | `no dbt project at <root>: its dbt_project.yml is missing, is not YAML, or names no project` |
| `Unindexed` | `dbt parse has not finished: <why>`, why `it is running`, the parse's failure, or `it has not run, or left no manifest` |
| `Refused` | a rename's refusal, as written |
| `Elsewhere` | `<document> lies in the dbt project at <project>, and this session serves only the one at <root>` |

A root whose `dbt_project.yml` no longer reads (removed, broken
mid-edit, left without its `name`) is `Unreadable` for a definition or
references on a target, workspace symbols and the call hierarchy, where
no target stays empty.  Once a parse has given an index, a hover and a
nodeInfo at a `ref`, a `source` or an entry's `name:` are `Unreadable`
there too.  Inlay hints stay `[]`: their lookup reads the project.
Before an index, nodeInfo fails with the parse's reason (`dbt parse has
not finished: ...`), and so does a hover at a `ref`, a `source` or an
entry's `name:`.  A command's failure, a result with `error` set, keeps
its own form.

## Capabilities

Each method the server answers has a capability (`src/capability.rs`,
plan 042): one descriptor per method or command for clients, which says
whether its nothing is `[]`, whether it wants the parse's index, and
whether it reads the project.  Routing and the reference's table of
methods and commands come from that one table, and a unit test holds the
reference's table to it.  The object announced is written beside it
(plan 083); a unit test holds its providers, each offered, to a list of
their names and to the number of method rows, and its commands to the
command rows.  The command line takes `-h` and
`-V` alone; any other option is a usage error.

## Commands for clients

The commands for clients (`src/commands.rs`, plan 022) do what dbt lsp's
undocumented commands `dbt.compileFile` and `dbt.listNodes` do, by dbt's
CLI.  Their answers are dbt lsp 2.0.9's shapes, the contract:

| Command | Arguments | Answer |
|---|---|---|
| `dbt-lsp-plus.compile` | `[URI]` | `{"file_uri": URI or null, "error": TEXT or null}` |
| `dbt-lsp-plus.nodes` | `[SELECTOR]`, dbt's syntax | `{"nodes": [NODE], "error": TEXT or null}`, each node with at least `unique_id`, `name`, `resource_type`, `package_name`, `original_file_path`, `depends_on.nodes` |

A selection of no node is `{"nodes": [], "error": "No nodes found"}`.
The first argument alone counts, a string, as dbt lsp reads its
commands'.  The server's own failures hold these keys alone, `error` set
and the rest null (`[]` for nodes); arguments it cannot read are -32602,
and after `shutdown` -32600, as every request.  `initialize` announces both.  A
server command with no `lsp`, from which a run's is made, makes the run
answer an error.  The command for previews, which only dbt lsp answered,
went with it (plan 075): `dbt show` alone would reach the warehouse, a
new feature, and a request of it gets -32601 (see "History").

The runs go one at a time, in the order asked, in `CACHE/run/`, apart
from the parse's target, its manifest removed before each run so that only
this run's counts, the compiled files of earlier runs kept for the
session; with the parse's flags, working directory, environment and guard
on packages; each killed past two minutes, at `exit`
and when the client closes; a run that ends with no answer, as on a panic,
is answered with a failure.  A compile runs `dbt compile
--select path:R --log-format json`, R the file's path from the root, the
root's own path never counted; a file outside the root, in a package, or
whose R holds a space or a comma (dbt's selector would split it) is
refused.  So is a compile where a package may have on-run hooks, which
`dbt compile` runs against the warehouse, even for one model (below):
`commands::hooked` reads the `dbt_project.yml` of the root, then of each
package under the install path that the guard takes (the server
command's `--packages-install-path`, else `DBT_PACKAGES_INSTALL_PATH`,
else the project's), by directory, as dbt loads them once the guard lets
it run, and names the first whose `on-run-start`, `on-run-end` or
top-level merge key, which may bring one, holds anything but a null, `""`
or `[]`, the forms seen harmless: `NAME has on-run hooks, which dbt
compile would run against the warehouse`.  A `dbt_project.yml` there that
repeats a key, of which dbt reads the last, refuses first, hook or none,
in words of its own, true whichever value dbt reads: `FILE repeats a
key, of which dbt reads the last: a hook may hide there`; one that does
not read refuses with the guard's words.  Its answer is the `compiled_path` of the node of the root package
whose `original_file_path` is R, a test's never (as of a properties file),
in the manifest the run wrote, resolved from the working directory; none
answers dbt lsp's "Compiled file path not
found."; a failed run, dbt's first error (below).  `nodes` runs
`dbt list --select=SELECTOR --output json` with dbt's text log and
`--quiet`, and answers its JSON lines with a `unique_id`.  Either answers
dbt's first error, whatever its exit status, by one rule for either kind
of log, since dbt may write the kind its format does not name (below):
the first line that is an error event, its message, or that begins
`[error] `, without that prefix (`run::Output::first_error`), as the
parse's failure does too; else, for a failed run, the first line that is
none of dbt's events, as a launcher's own, with how the run ended.  The
compiled file lives while the session does, as the cache.

### What dbt does for these commands, observed on 2026-11-17

Offline (`unshare -rn`), a scratch home, a copy of the fixture, both builds
alike unless said:

- dbt lsp 2.0.9: `dbt.compileFile [URI]` answers `{"error": null,
  "file_uri": "file:///…/target/compiled/shop/models/marts/orders.sql"}`,
  and on a file of no node `{"error": "Compiled file path not found.",
  "file_uri": null}`; `dbt.listNodes ["+stg_orders+"]` answers
  `{"error": null, "error_kind": null, "grain": "project", "nodes":
  [...]}`, each node with its `unique_id`, `name`, `resource_type`,
  `package_name`, `original_file_path`, `depends_on.nodes` and `config`
  (`access`, `group`, `materialized`), and an empty selection `{"error":
  "No nodes found", "error_kind": "lineage_query_failed", "grain":
  "unknown", "nodes": [], "root_project": "shop"}`.
- `dbt compile --select path:R`, with the proxy's flags and the working
  directory elsewhere, compiles
  statically and writes the node and the tests on it (`*.sql`,
  `*.macro_spans.json`); its manifest records the node's `compiled_path`
  relative to the working directory.  A snapshot's file is
  `compiled/shop/snapshots/orders_snapshot.sql/orders_snapshot.sql`; a
  version's, `compiled/shop/models/versioned/dim_v2.sql`; a seed has no
  `compiled_path`; a disabled model, a macro's file, a file of no node and
  a file under `dbt_packages/` select nothing (a warning, status 0).  A
  model with `run_query` under `execute`, and a Python model, try the
  driver and fail offline.
- `dbt list --select=S --output json --quiet` prints one JSON object per
  node; `*` gives models, seeds, snapshots, analyses and tests, no source,
  as `dbt.listNodes` does, `source:*` the sources, `+dim.v2+` the version
  alone, an empty selection nothing, a broken project `[error] [CODE]:
  MESSAGE`; with `--log-format json` every line is a log event.
- `dbt show` and `dbt build` fail offline: dbt fetches the DuckDB driver
  from its CDN when the home has none.  `dbt show --help` (2.0.11) gives
  `--limit` "[default: 10]".
- Observed on 2026-11-22, by plan 048's review, on dbt 2.0.9 and
  dbt-oss 2.0.11 alike, offline in a scratch home: a log may hold the
  kind its format does not name.  With `DBT_LOG_FORMAT=json` in its
  environment and no `--log-format`, `dbt list --select=S --output json`
  logs JSON events alone: with no `profiles.yml`, the error event
  `[error] [InvalidConfig (dbt1005)]: No profiles.yml found at …`, no
  other line, and status 1.  With `--log-format json`, a target path that
  dbt cannot create, its log path writable, gives the bare line `[error]
  [IoError (dbt1001)]: Failed to create output directory: …`, no event,
  and status 1; a project directory that cannot be canonicalised does
  the same.  On 2026-11-24, by plan 082, offline on a copy of the
  fixture, `dbt list --select '*' --output json --quiet` printed 11 node
  lines on dbt-oss 2.0.11 and on dbt 2.0.9 alike, whose keys are
  `alias`, `config`, `depends_on`, `name`, `original_file_path`,
  `package_name`, `resource_type`, `tags` and `unique_id`: no `info`, so
  no node line reads as an error event, and one rule reads both kinds
  for either command.
- Observed on 2026-11-18, offline, on both builds: `dbt compile` stopped
  at an `on-run-start` or `on-run-end` hook given as a string, a list, a
  package's, even `"{{ '' }}"` (dbt1308, the driver), and did nothing
  more for `[]`, `""` or no value.  Observed on 2026-11-22 on a
  Snowflake trial account with dbt 2.0.9: `dbt compile`, also with
  `--select` of one model, ran the `on-run-start` and `on-run-end` hooks
  against the warehouse; `dbt parse` and `dbt list` sent no query.
  Observed on 2026-11-23, by plan 056's review, with dbt-oss 2.0.11
  offline: `on-run-end: ~` and `on-run-start: null` ran nothing, as no
  value does; a package with an `on-run-start` hook under the server
  command's `--packages-install-path` made `dbt compile` start the hook
  (dbt1308, `in <RUN_OPERATION>`).

## Root

The project directory, the root, comes by one of three routes, the first
that applies:

- **given**: the server command's `--project-dir` (`--project-dir X` or
  `--project-dir=X`).  The dbt is the command as written, unprobed, run
  in the server's working directory; the line `dbt-lsp-plus: project
  <root>, dbt <words before lsp> (the server command).` goes on the
  standard error at the start and to the client once it is initialized (plan 075).  dbt-mode
  takes this route, always.
- **the folder**: else the client's first workspace folder, or `rootUri`,
  when it holds a `dbt_project.yml`, taken when the client's `initialize`
  comes, with the dbt (below).
- **a document**: else the first document opened that lies in a project,
  from its path as spelt: the nearest directory above it holding
  `dbt_project.yml` that lies in no package of a directory above it
  holding one (`locate::project_of`).  A directory lies in a package of
  one above when it is under that one's install path, as dbt takes it, or
  in one of its local packages, as spelt or links resolved, as a hub
  package's own `integration_tests` project does; the walk then goes on
  from the next one above.  A project whose `dbt_project.yml` does not
  read has no packages.  No walk stops at git's
  top level: a worktree in a checkout is no package of it.  dbt-mode's
  walk (`dbt-project.el`) also skips `dbt_internal_packages` and reads the
  install path from the project alone.  The server answers `initialize`
  before any document comes (Claude Code opens the first one right after
  `initialized`).  At that document, before it is tracked: the root, the
  dbt, the first scan, the watcher of runs, the watcher of files when the
  client lets the server register it, and the line; then the parse.

The working directory no longer names the project.  Before a project,
and for ever when no document lies in one, the server is quiet: no
process (no probe, no git, no dbt), no polling, no watcher, nothing on
its standard error, no message, and every request of a method or a
command of its own gets null, valid for each method it announces and
each one an agent was seen to send; an unknown one gets -32601.

Once there is a project, a document lies in it when under the root or
in one of its packages, installed or local, as spelt or links resolved,
and in another project when the walk up from it finds one
(`locate::elsewhere`).  A request on a document of another project, of a
method that reads the project and names its document by
`textDocument.uri` (a hover, a definition, references,
`prepareCallHierarchy`), fails with `<document> lies in the dbt project
at <project>, and this session serves only the one at <root>`, and so
does a nodeInfo at a position there, before or after the first parse.
Inlay hints there are `[]`.  The rest is answered as
before: the methods of the document's own text, a rename, which there
is a CTE's alone, a document in no project, every request while the
root's `dbt_project.yml` does not read, and a project nested under the
root, which is the root's: one session serves one project (plan 049).

On the folder's route and a document's, when the server command's first
word is `dbt` (the default `dbt lsp` included), the dbt is the project's
(`locate::dbt`): the first of `<project>/.venv/bin/dbt`,
`<top>/.venv/bin/dbt`, `<main>/<project from top>/.venv/bin/dbt` and
`<main>/.venv/bin/dbt`, without repeats, that is a file and answers `lsp
--help`, in the project, as a dbt 2 does, with its help (status 0) or
dbt-oss's refusal (status 2 and "unrecognized subcommand 'lsp'"), the
probe's one use since plan 075, which tells a dbt 2 from anything else; `<top>` and
`<main>` come from one `git rev-parse --show-toplevel --git-common-dir`
in the project, without `GIT_DIR` or `GIT_WORK_TREE`, `<main>` the common
directory's parent when that is named `.git`; the common directory, which
git may give relative to where it ran, is read from the project as
written, each `..` dropping the component before it, so that a `.git`
reached through a link keeps its name; with no `--path-format`, which git
before 2.31 prints back as a word.  A file that answers otherwise (dbt-core 1.x's "No such command
'lsp'", one that cannot run, one past the deadline) is skipped and named.
Else `dbt` stays, for `PATH`, unprobed.  Any other
first word (a path, `uvx`, `dbt-2.0.9`) is the user's, kept.  Only `.venv`
is looked at: no other environment, no `Scripts\dbt.exe`.

The line: once, on the standard error and as one `window/logMessage`
(type 3) once the client can take it, `dbt-lsp-plus: project <root>, dbt
<path> (<where>)`, `<where>` one of "the project's .venv", "the
repository's .venv", "the main worktree's project .venv", "the main
worktree's .venv", "PATH" (named by the first `dbt` on it) and "the server
command", then each place skipped and why, then a period.  It is not an
interface (`doctor` is).  Claude Code shows it only in its debug log.

On those two routes, dbt's processes run in the root: the probes, the
parse and the runs of the commands for clients, so that a `profiles.yml`
beside the project is found as under lsp-mode, which starts the server
there; a YAML error's file and a compile's path are read from there too.
The server's own working directory does not change, and what
the server command names from it keeps naming it: a first word that is a
relative path and a relative `--profiles-dir` are made absolute against
it first (`config::anchored`); a relative `--packages-install-path` is
the project's, for dbt as for the server.

The URIs the server returns are built on the root, so the root keeps the
spelling the user or the client gave it:
- `--project-dir` is made absolute as spelt, links unresolved, each `..`
  dropping the component before it; it is resolved only when that would name
  another directory, through a link before a `..`;
- when the client's folder is the same directory under another spelling (a
  symbolic link), the client's spelling wins.

A document under the root as spelt keeps its path, links within the project
included, since the scan walks them so.  A document reached through another
spelling of its directory is mapped onto the root by that directory's
resolved path.

### What Claude Code does with a language server, observed on 2.1.317

A logging language server in a plugin, and Claude Code driven by a local
stand-in for the Messages API that plays a script of tool calls, in a
scratch home and configuration, on Linux, from the command line: at a
repository's root whose dbt project is in `dbt/`, in a linked worktree,
in `dbt/models`, with no capability announced, an `Edit` first, and
dbt-lsp-plus 0.1.5, then a proxy with dbt 2.0.9's `dbt lsp` behind it.  What was read from the 2.1.317
bundle's code rather than seen in a session says "(bundle)".

- A server starts late: nothing starts with the session.  The first call
  of the LSP tool on a file whose extension the plugin maps starts it, and
  so does the first `Edit` of such a file; `Read` does not.  The LSP tool
  is deferred: an agent loads it with ToolSearch first.
- Its process runs in the session's directory (or a host's
  `projectConfigRoot`, or `.lsp.json`'s `workspaceFolder`, when set
  (bundle)); `${CLAUDE_PLUGIN_ROOT}` and `${CLAUDE_PROJECT_DIR}` are
  expanded in `args`; its environment is Claude Code's, plus
  `CLAUDE_PROJECT_DIR` (the session's directory), `CLAUDE_PLUGIN_ROOT` and
  `CLAUDE_PLUGIN_DATA`.  The desktop app's was not observed.
- `initialize` names one workspace folder, `rootUri` and `rootPath`, all
  the session's directory: a repository's root, a worktree's root or
  `dbt/models`, never the nested project.  `initializationOptions` is `{}`.
  The client offers `didSave`, hover in markdown, definition links,
  hierarchical document symbols, the call hierarchy and
  `positionEncodings: ["utf-16"]`; `workspace.workspaceFolders` false,
  `didChangeWatchedFiles` without dynamic registration; no rename, inlay
  hints, code lens, code actions, completion, `executeCommand` or
  `window.workDoneProgress`.
- The first `didOpen` comes at once: `initialized`, the `didOpen` of the
  file that started the server, then its request, within a millisecond of
  the answer to `initialize`.  Each new file is opened before its request.
  The first document can lie outside every dbt project: an `Edit` of a
  repository's `ci.yml` opened it before any model.
- Announced capabilities are not read: to a server that announced none,
  it sent hover, `workspace/symbol`, `textDocument/implementation` and
  `prepareCallHierarchy`.
- What it drops: `window/showMessage` and `window/logMessage` leave no
  trace; a line on the server's standard error goes to the debug log,
  written only with `--debug` or `--debug-file`, as `[LSP SERVER
  plugin:<plugin>:<server>] <line>`; `client/registerCapability` is
  answered null and ignored (bundle); `didChangeWatchedFiles` never comes.
- An `Edit` of a file not yet open sends `didOpen` with the new text and
  no `didSave`; of an open file, `didChange` with the whole text, then
  `didSave`.  Past 50 open files it closes the oldest (bundle).
- References outside the session's directory, or under a path git
  ignores, reach the agent; a null reads "No symbols found", "No
  definition found".  A request waits 60 s by default (`requestTimeout`);
  a crashed server is restarted up to three times (bundle).
- `.lsp.json` (bundle): `command` (no space unless absolute), `args`,
  `extensionToLanguage` (required), `transport`, `env`,
  `initializationOptions`, `settings`, `workspaceFolder`,
  `startupTimeout`, `shutdownTimeout`, `requestTimeout`, `restartOnCrash`,
  `maxRestarts`, `diagnostics` (default true: diagnostics pushed into the
  agent's context after its edits).  An extension maps to one server: a
  second plugin claiming `.yml` is not used for it.  With `--plugin-dir`, a
  directory with `.lsp.json` at its top is one plugin (observed).
- dbt-lsp-plus 0.1.5 with dbt lsp 2.0.9 behind it, offline, the session
  in the fixture's folder: a hover on `ref('stg_orders')` answered dbt
  lsp's parents and children, then, once the parse had run, the summary
  above them; the definition, one reference, eight workspace symbols.
  Claude Code answered dbt lsp's `window/workDoneProgress/create`,
  `workspace/configuration` and refresh requests, and took 143 empty sets
  of diagnostics.
- The LSP tool's `line` and `character` are 1-based, sent as `line - 1`
  and `character - 1`: line 1, column 1 is position 0:0 (bundle).
- An error answer reaches the agent as `Error performing <operation>:
  LSP request '<method>' failed for server 'plugin:<plugin>:<server>':
  <message>`: the message whole, not its code nor its `data` (bundle).
- ContentModified (-32801) is retried up to three times, after 500, 1,000
  and 2,000 ms, then reported as any error; no other code is retried
  (bundle).
- A null hover reads "No hover information available", with "the cursor
  is not on a symbol" or "the LSP server has not fully indexed the file"
  as its causes; a null definition, empty references and empty workspace
  symbols read alike, each naming indexing as a possible cause (bundle).
- A workspace symbol shows as `<name> (<Kind>) - Line <n> in
  <containerName>`, grouped by file, its `data` unread; a call hierarchy
  item as `<name> (<Kind>) - <file>:<line> [<detail>]`; a document symbol
  as `<name> (<Kind>) <detail> - Line <n>`.  A hover's contents are
  joined, prefixed with "Hover info at L:C:" only when it has a `range`
  (bundle).

So a plugin cannot name the project, and when the client's folder holds
none, the server answers `initialize` before it can know it.

## State

- Open documents: the text of each, kept by the Workspace
  (`Workspace::open`), which the LSP front end writes from `didOpen` and
  the incremental `didChange` (UTF-16 units), so that the outline follows
  unsaved edits.
- The project: found on each request from `dbt_project.yml`, its resource
  paths, through symbolic links, the packages under the install path
  that the guard takes too (`packages::install_path`: the server
  command's `--packages-install-path`, else `DBT_PACKAGES_INSTALL_PATH`,
  else the project's `packages-install-path`), and the `local:` packages
  of `packages.yml` and `dependencies.yml` not installed there yet, as
  dbt reads them, with the open texts winning over the files on disk.  A file whose name starts
  with a dot is left out, as dbt leaves it out, but not the files of a
  directory whose name does.  Seeds are found but not read.  This finds:
  - models, Python models among them, seeds, snapshots, analyses,
    singular tests, and functions, each `.sql` or `.py` file under
    `function-paths`;
  - snapshots declared in YAML: a `snapshots:` entry with `relation:`,
    its calls those of its relation; one without patches a snapshot of
    SQL, or names nothing;
  - each version of a versioned model, at its file as dbt finds it: the
    version's `defined_in`, else `<name>_v<N>`, anywhere under the model
    paths, and the latest's `<name>` too unless a `defined_in` names it.
    The latest is `latest_version`, else the highest version: as numbers
    when all are numbers, else all as text, as dbt orders them;
  - macros, generic tests and docs blocks;
  - source tables, from the YAML of every resource path, parsed with the
    place of each scalar as written and where each mapping ends;
  - the `ref`, `source`, `function`, `adapter.dispatch` and macro calls
    inside the Jinja tags of each body.  A tag inside a SQL comment counts, since Jinja
    renders it; a `ref(` in prose does not.
- The scan of SQL and Jinja is lexical, not a parser:
  - Jinja tags end at their first closer outside strings and nested braces,
    and the content of a raw block holds none;
  - CTEs are found after `with` or a comma, outside Jinja, SQL comments and
    strings, so a CTE that Jinja generates is not one but those after it
    are;
  - names may hold any letter.
- The scan is kept between requests by the Workspace
  (`src/workspace.rs`), the core's state of the session's project beside
  the last parse's index, the parse under way and the call hierarchy's
  graph, and the open texts, which speaks no protocol: the LSP front end
  keeps the URIs and the positions, and writes the open texts.  It
  lives on the event loop, which alone owns it (`src/cache.rs`).  A
  request that needs the project walks the resource paths again and
  reads each file's metadata before its content:
  its modification and change times, size, inode and device.  A file whose
  metadata stays keeps its text and what that text alone gives (blocks,
  calls, source tables); any other is read again, and a file gone is
  dropped.  A file that is not UTF-8, such as a `Thumbs.db`, counts as
  empty; one that cannot be read counts as empty too, and is tried again
  at the next request.  A seed is found but never read, open or not.  An
  open document is known by a hash of its text, since a client's version
  starts again at each open.  The cache keeps one scan per file, so a
  text edited back reuses it only if no request read the project in
  between.
  No file event is needed: a client may watch nothing.
  - When no file changed, nor any package's name and paths, which each
    `dbt_project.yml` gives and the walk reads again, the request shares
    the last project.  Otherwise the project is assembled again from the
    kept files, since a file's resources depend on its package and
    layout.  The call hierarchy's graph is kept while the project and the
    last parse's index stay.
  - `initialize` starts a thread that reads every file once, whose files
    the cache takes once it is done.  Nothing waits for it: a request
    that comes before reads the files itself.
  - Measured on synthetic projects (`scripts/bench/`) of 3,000 and 6,000
    models with 15 YAML columns each (12.8 and 22.8 MB), on tmpfs, in a
    release build: a workspace symbol, definition, references, rename or
    call hierarchy request answers in 15 to 23 ms and 30 to 40 ms with
    nothing changed, but the references of a macro that every model
    calls (see "Known limitations"), and within 0.20 and 0.31 s as the
    first request after the save of a model, of the largest properties
    file or of `dbt_project.yml`.  Each took 0.3 to 1.7 s without the
    cache.  The server then holds 79 and 142 MB.  These figures predate
    plan 050: a definition, references and the call hierarchy ask
    for the index, and so walk once more when it is fresh and a parse has
    started, to compare the files on disk with that parse's stamp
    (`Workspace::parse` asked to look), and each parse's start walks once
    more for its stamp (`cache::disk`); unmeasured.
- The graph: once a parse has given a manifest, the dependencies of each
  node it holds are the manifest's, since only dbt resolves
  `ref(var('x'))`; until then, and for the nodes it lacks, they come from
  the scan.  A manifest's edges to macros are transitive, so the edges to
  and from macros always come from the scan.  The scan looks a call up as navigation does (see
  "Navigation"), by the one rule of `navigation::Lookup`: a ref or a source
  in the caller's package, the root project, then any package, the root
  first under the lookup flag, every candidate where dbt fails on two
  packages (dbt1005) (in a macro, dbt looks in the calling node's package:
  see "Known limitations"), and an unqualified macro in the caller's
  package and the root project only, as dbt does: another package's macros
  need its name.  Once a parse has given an index, the scan's calls are
  looked up with it: a node or a source table that the last manifest
  disables is no call's target, a name the parse knows leads to its
  nodes, a call keeps its site on the edge that the manifest gives, and a
  target no file holds is left out.  A dispatch names every `*__name`
  implementation of its package.
- A source that `overrides` a package's source is that package's source:
  dbt-core patches it, so the server places its tables in the package
  (dbt 2 does not: see "Known limitations").
- Text read from disk loses its byte order mark, as an editor's document
  does, so that positions on its first line match.
## Sources

- References of a source table, or of a whole source: every `source()`
  call that names it, in SQL inside Jinja tags, in YAML without them (a
  relationships test's `to:`), in each package's `dbt_project.yml` (vars,
  hooks) and in a Python model (`dbt.source()`); the key that configures it
  under `sources:` in the root `dbt_project.yml`; and its declarations when
  the client asks.  A call whose argument is an expression, as in
  `source('raw', 'events' ~ var('suffix'))`, names no table.
- References come from the scan whichever file under the root asks, the
  asking file left out when the scan does not hold it, as a file not saved
  yet or one outside the resource paths.  A rename is answered only on a
  file the scan holds, which it then edits; elsewhere it goes to the
  server.
- An open document is named by the URI the client opened it under, and its
  ranges are those of its open text.
- A rename keeps the relation in the warehouse.  dbt takes a table's
  identifier from its name, and a source's schema from its name, unless they
  are set: `dbt parse` gives `events.page_views`, declared without `schema:`,
  the schema `events`.  So renaming a table without `identifier:` inserts
  `identifier: <old>` after its `name:` line, at the key's indentation and
  with the line's own line break, and renaming a source without `schema:`
  inserts `schema: <old>`.  The live test renames `events.page_views` and
  checks that dbt still compiles the model that selects from it to
  `page_views`.
- A name inside quotes is replaced inside them, and must not hold that quote
  or a backslash.  An unquoted YAML value is written plain when it is a plain
  identifier that YAML reads as a string, else double-quoted: `null`,
  `true`, `false`, `yes`, `no`, `on`, `off`, `y` and `n`, in any case, are
  quoted.
- A rename is refused, never applied in part, when:
  - the source is declared nowhere, or in several packages;
  - it is declared in an installed package, or an installed package calls
    it: `dbt deps` would undo the edit, and a root `overrides:` would keep
    the old name;
  - its properties file holds an alias or a merge key anywhere, so that a
    name, a schema or an identifier may come from elsewhere;
  - the source already declares the new name;
  - a line must go into an entry in flow style (`{name: t}`), a JSON file
    included;
  - an edited YAML file would no longer parse, as when a flow collection
    spans lines that look like blocks, or a new name with `: ` goes into a
    plain `to:` value;
  - the new name is empty, or breaks the quotes it goes in, or, where a call
    sits in a YAML value such as a test's `to:`, is no plain identifier:
    that value may be a plain scalar, where `#`, `: `, `,` or a space at an
    edge would change it while the file still parses.
- Elsewhere, references answer `[]` and a rename null.

## The last run as diagnostics

After the user's own `dbt build`, `run`, `test`, `seed` or `snapshot`, from
an editor, a terminal or an agent's shell, each node that errored and each
test that failed or warned shows as a diagnostic where it is written.  A
thread looks at `run_results.json` and `manifest.json` in the target path
(`DBT_TARGET_PATH` in the proxy's environment, else `target` under the
root; dbt 2.0.9 refuses `target-path` in `dbt_project.yml`) every second,
by size and time, and reads them when either changed, off the loop, a file
that does not read as JSON being read again when dbt changes it.  Only a
`build`, `run`, `test`, `seed` or `snapshot` replaces the failures, a
partial one included, as dbt's file does; any other command's results
(`compile`, `show`) leave them as they are.  Results that are gone, as
after `dbt clean`, leave no failure.  Each result's id is looked up in the manifest there; an id it lacks
gives nothing.

- A node's error: Error at its file's first line, dbt's message.
- A test that failed: Error, `<test> failed: <n> rows`; warned: Warning;
  errored: dbt's message; a unit test that failed or errored: Error.  A
  generic test at its name under its node's entry in the properties file
  (a model, seed or snapshot; for a test on a source, the source its
  `depends_on` names and its table), then its column's (a quoted column's
  quotes left out; of a versioned model's columns, the one holding the
  test), a test of the entry itself and not of one it holds; else at the
  entry's name; a singular test at its file's first line; a unit test at
  its entry; a function at its file.  A package's node at its file in
  that package.
- Each message starts with the run's command, `dbt build: ` (its
  `args.command`, else `args.which`).
- `source: "dbt-lsp-plus"`, `code: "last-run"`, no `version`.

After the user's own `dbt source freshness`, a second thread watches
`sources.json` the same way, the manifest read only when there are results
to place: each table found stale shows at its `name:` under its source, a
Warning, or an Error past `error_after`, with `code: "stale-source"` and a
message such as `dbt source freshness: raw.orders last loaded
2018-01-02T00:00:00Z, 3237 days ago (warn after 1 day)`, the age in days,
else hours, else minutes.  dbt 2.0.9 writes `sources.json` (schema
sources/v3) with capitalised statuses (`Warn`, `Error`, `Pass`),
`max_loaded_at`, `max_loaded_at_time_ago_in_s` and `criteria`
(`warn_after`, `error_after`, nulls where unset), and no result at all for
a table whose query failed, which then shows nothing; dbt-core's `runtime
error` shows as an Error with its `error`.  Each kind is replaced only by
its own file: the check rewrote `run_results.json`, as `freshness`, in one
probe where it reported a model's error, and left it alone in another.

The places are found again in the file's text, open or on disk, at each
publication, its entries and tests read once.  A file the client changes
or saves after the run loses its failures, as does one whose file is newer
than the results; when the last parse has problems in it (not an empty
set), the server publishes nothing then, since their ranges are of the
text before the edit, which the client has moved, and the next parse's
publication goes out without the failures.  A file's failures and its
parse's problems go out together, under the URI the client opened it
under, else the file's; the server keeps the URI it last published each
file under and first clears it when the URI changes.  Nothing after
`shutdown`.

## Navigation

The server answers a definition, and references, where the document
alone shows one of these targets, in any file, a Markdown docs block's
Jinja included, and gives its nothing elsewhere:

- `source`: a `source()` argument in any file, or a source's or table's
  `name:`, leads to the table's or source's `name:`;
- `ref`: a `ref()` argument in any file, a Python model's `dbt.ref()`,
  or a unit test's `model:`, leads to the model's, seed's or snapshot's
  file, a version's included;
- `macro`: a macro call's name or package, in SQL or in the Jinja of
  a YAML value such as a hook, or the name in the tag of a macro or a
  generic test (the macro `test_<name>`), leads to the macro's whole block,
  as dbt lsp answers; the `ref` and `function` keywords themselves are
  macro calls, since a project may override them; dbt's own macros
  (`dbt.*`, builtins) lead nowhere, and a materialization's tag names
  nothing;
- `function`: either argument of `function()`, in SQL or in the
  Jinja of a YAML value, leads to the function's file;
- `doc`: a `doc()` argument in a YAML file, its quotes as YAML escapes them,
  leads to the docs block; dbt has no `doc` in SQL, where `doc(` is a
  macro call;
- `entry`: the `name:` of a model, seed, snapshot, analysis, macro or
  function entry leads to its file or block, each version's file for a
  versioned model;
- `test`: a generic test's name in a `data_tests:` or `tests:` list below
  the top level (an item, the one key of an item, or `test_name:`) leads
  to its test block; dbt's own tests have none, and get null.

Lookup follows dbt 2.0.9, probed with `dbt parse`, by one rule for
navigation, references, the call hierarchy, hover, nodeInfo and inlay
hints (`navigation::Lookup`): a package the name gives is the only one
searched; else a `ref` or a `source` tries the file's own package (the
installed package whose directory holds it), then the root, then any, the root first when `flags:
require_ref_searches_node_package_before_root` is a YAML 1.2 false, as dbt
2.0.9 and dbt-oss 2.0.11 read it: a plain `false`, `False` or `FALSE`, not
`fAlse`, `no` nor a quoted `'false'`, and no parse warns of a value it
ignores; an unqualified macro call tries the file's own package, then the
root, never another package, whose macros dbt keeps behind its name, and
inside a package's macro that is the macro's package, where dbt looks in
the rendering node's (the owner's choice, plan 017); a `doc` tries the
root, then any package, whichever file calls it and flag or not: the
first by the byte order of package names, which is the order of their
paths, since dbt installs each package at `dbt_packages/<name>`; an entry
describes a resource of the file's own package only, since dbt 2.0.9
calls an entry naming another package's resource unused (dbt-core 1.12
patches it).  As dbt-core 1.12.6's parser reads them: a generic test, the
file's package then the root; a unit test's model, the file's package.
The first package that has something gives its every candidate: one, but
where dbt fails on a name that two packages other than the file's and the
root define (dbt1005), every one, as the owner chose; and every
declaration of a source.

A `ref` that names a version (`v=` or `version=`) leads to that version's
file, one that names none to the latest version's, nothing when that
version has no file, as dbt fails then, and a version of a model without
versions to nothing; an entry of a versioned model, or a unit test's
`model:` of one, leads to each version's file, as dbt patches and tests
them all.  The scan knows these; once a parse exists, the parse's nodes
decide a name it knows, and skip a node that the root disables, so that a
package's disabled model gives way to the root's.  Only the last manifest
counts, as for the graph (see "State").  A definition or references
request starts a parse when the index is missing or stale, and answers
from the scan meanwhile.  A function call, `function('f')` or `function('pa', 'f')`,
is looked up as a `ref` is, flag included, from the scan alone, as the
manifest keeps functions apart from its nodes; a `functions:` entry's
`name:` leads to the function's file.  A location is the name of a block
or of a `name:`, a snapshot's even once parsed, in its block or its
properties file, a macro's whole block, or the start of a model's or a
function's file, as dbt lsp answers a `ref`.

References at a `ref()` or `function()` argument, a unit test's
`model:`, a macro call's name, or the name in the tag of a macro or a
generic test: each
call that resolves to the same resource, as its definition would, at its
name, in SQL, YAML (a relationships test's `to:`, a value's Jinja,
`dbt_project.yml`'s hooks), Python (`dbt.ref`), analyses, snapshots and
tests; a unit test's `model:`; for a generic test, its uses in properties
files.  With `includeDeclaration`, the file's start or the macro's block,
and the `name:` of each properties entry declaring it in its own package.
The calls come from the kept scan, which keeps a YAML file's macro calls,
its unit tests' models, its resources' entries and its generic tests, read
from one parse, so that no request parses YAML again; calls alike from one
package are looked up once.

A definition or references request reads the document's open text if
the client opened it, else the saved file, since a client may send a
request on a file it never opened; a position that names no target in
that text, and a document that is neither open nor on disk, get its
nothing.

Hover there extends the hover summary (see "Node information and hover"):
on a `doc`, the docs block's markdown as written, if it holds any; on a
generic test, its opening tag and its package, in a code block for a
client that reads markdown; on an entry's `name:`, its
node's summary, which `nodeInfo` with that position gives whole.

## Within a document

Four capabilities need one document alone, and so does the rename of a
CTE:

- Outline: in SQL and Markdown, the Jinja blocks and the CTEs, nested by
  extent: the definition blocks (`macro`, `test`, `snapshot`,
  `materialization`, `docs`, the block `set`) by their name, and the
  `if`, `for` and `call` blocks by their statement as written, from the
  keyword, its trim markers dropped and each run of spaces, tabs and line
  ends one space (`if is_incremental()`, `call(user) dbt_utils.m()`),
  kind 3 (Namespace) with the detail `if`, `for` or `call`, as agreed with
  dbt-mode for its breadcrumbs (plan 027); `elif`, `else` and closing tags
  are no symbols, nor a tag in a raw block.  A control block left open
  runs to the end of the innermost block or CTE that holds its opening
  tag, else of the text, so that the nesting holds while it is typed.  Only the outline names
  the control blocks: a CTE's scope and the project's resources stay the
  definition blocks'.  In a `.yml` or `.yaml` file, the named entries of a properties file: those under `models`, `seeds`,
  `snapshots`, `analyses`, `macros`, `functions`, `data_tests` (or
  `tests`), `sources`, `exposures`, `metrics`, `semantic_models`,
  `saved_queries`, `unit_tests` and `groups`, with the `columns` of a
  model, seed, snapshot, analysis or source table, a model's `metrics`
  and the `tables` of a source.  The detail is what the entry declares
  (`model`, `table`, `semantic model`); the kinds are those agreed with
  dbt-mode.  A node shows its resource type's kind wherever the proxy
  lists it, the one its entry has here (`project::TYPES`): in workspace
  symbols and the call hierarchy too.  So a unit test is an Event and an
  exposure an Object in the call hierarchy as here, where only the
  manifest brings them; workspace symbols never list either.  A doc,
  which no entry declares, is a String (15) as its docs block is here,
  and a type the table lacks a Class (5).  A source table is an
  Interface; a source, the group of its tables, which is no node, a
  Module.  An entry runs from its first key to its last content: the
  blank lines and the comment lines less deep than its first key that
  end it belong to what follows, while a key commented out and a block
  scalar's deeper lines stay in; in flow style it ends at its brace.  An
  entry without a name, such as one whose `name:` is being typed, is no
  symbol, and aliases are not followed.  Other YAML, such as
  `dbt_project.yml`, has no entry, nor does YAML that does not parse.
- Folding: every Jinja block (`if`, `for`, `call`, `filter`, `macro` and
  the others), CTEs and multi-line Jinja tags (`region`), and multi-line
  `{# #}` comments and, in SQL, `/* */` comments (`comment`), by line.  A
  closer alone at the start of its line stays visible, its whitespace
  control (`-}}`) included; a tag or comment left open runs to the end of
  the text.  Of the ranges that start on one line, the longest comes first,
  since some clients keep only the first.
- Highlights: on a CTE, its definition (write) and its uses (read); on a tag
  of a Jinja block, every tag of the block, `elif` and `else` included.
- Selection ranges: the word, the Jinja tag, a CTE's or block's content,
  the CTE or block, and the whole text, each containing the one before.
- Folding and selection ranges are for SQL and Markdown: on a YAML file the
  proxy answers null, so that a client that falls back on null, as VS Code
  does, folds and selects by indentation; one that does not gets nothing,
  as it got before the proxy announced folding.
- CTE rename, in any file: the definition and every use, whole words
  (whose neighbours are no letter, digit, `_` or `$`) outside Jinja,
  comments and strings, within the innermost block that defines the CTE,
  not after a `.`, matched without regard to case unless quoted.  A new
  name must be a plain identifier and not another CTE's.  A rename is
  refused when the name appears in a string inside a Jinja tag, as in
  `is_positive('orders.amount')`, since a macro may render it as SQL; the
  arguments of `ref` and `source`, which name models and sources, and the
  names of functions do not count.  A name a macro builds, as in `'ord' ~
  'ers'`, cannot be seen.

## Node information and hover

The files alone give neither relations nor resolved configuration, so
the server runs the user's own `dbt parse`.

### What `dbt parse` does, observed on 2.0.9

- With `--target-path` and `--log-path` under a cache, it writes its
  manifest, `semantic_manifest`, `snapshots/`, `generic_tests/` and logs
  there, and an empty `~/.dbt/leases/` in the home: 0.25 to 0.4 s on the
  fixture, 2.1 s on 2,000 models and 4,000 tests.  It runs beside a running
  `dbt lsp`.
- When `packages.yml` names a package that is not installed, it resolves
  packages as `dbt deps` does: online it writes `package-lock.yml` and
  rewrites `dbt_packages/`; offline it retries the hub for about 5 s.  No
  flag turns this off; `dbt lsp` does the same at its start.
- It sends telemetry on every run, even with `DO_NOT_TRACK=1`, unless given
  `--no-send-anonymous-usage-stats`, which `dbt lsp` takes too.
- Without `--profiles-dir`, profiles are looked up in the working
  directory, then `~/.dbt`, not in the project.
- A run that fails on a Jinja error, an unknown `ref` or invalid YAML exits
  1 and still writes a manifest of what it parsed; one that fails on the
  profile or an `env_var` writes none.
- The manifest's `relation_name` is quoted by the adapter, null for an
  ephemeral model and for tests, set for analyses and disabled nodes, which
  are never built; a disabled node is only under `disabled`; an owner is
  its group's; a source with no freshness has null counts under
  `warn_after` and `error_after`; a test on a source, or a singular test,
  has no `attached_node`, only `depends_on`; a composite `unique_key` is an
  array; each version of a versioned model is a node of its own.
- dbt-oss 2.0.11 parses as 2.0.9 does: for every project of the corpus
  (see "Conformance") the two wrote the same entries for the project's
  packages, and differed only in a few of dbt's own macros.  It
  has no `lsp`: `dbt lsp --help` exits 2, its standard output empty and
  `error: unrecognized subcommand 'lsp'` on its standard error, where
  2.0.9 exits 0 with its help.  Its `format --help` exits 2 too, but its
  `lint --help` exits 0.
- Offline, under `unshare -rn` with a scratch home, a parse of a DuckDB
  project needs no driver on either build, and with the flags above
  makes no connection attempt; without them, both builds try to connect,
  and on a host with nscd their name lookups cross the namespace through
  its socket.  `--write-json` is not needed: both write the manifest
  anyway.
- A package under `packages-install-path` is read whatever its directory
  is called, with no `packages.yml`; one that `packages.yml` names as
  `local: dbt_packages/<directory>` under another name makes dbt delete
  that directory and fail.  A root model and a package model of one name
  fail with dbt1005 on their database relation unless one sets another
  schema.
- A parse that fails still writes a manifest: on an ambiguous `ref` or
  `source` (dbt1005: two packages other than the caller's and the root
  define the name), with the node and without that call in its
  `depends_on`; on an unknown macro (dbt1501), without the node at all,
  though its dependents still name it.  dbt 2 ignores a source's
  `overrides` (dbt1060, an error) and declares a source of the root's
  own.  An entry naming another package's
  resource warns dbt1089, even with `--quiet`, and leaves that
  resource's `patch_path` null.  In the JSON log, a YAML loader's error
  (dbt1013, dbt1060) and dbt1001 name their file from dbt's working
  directory, the others from the project's root.
- A node's `depends_on` lists what rendering it reached: in `macros`,
  the macros it calls and those they call in turn (dbt's own included),
  but not through a dispatch; in `nodes`, its `ref` and `source` calls
  and those of the macros it reaches, resolved for that node.  A macro's
  own `depends_on` lists macros only.  Nothing lists a `doc()`, whose
  block shows only in the rendered `description`, nor a call in a branch
  that the parse does not render (`is_incremental()`).  A macro called in
  a YAML value is rendered and listed nowhere.  `adapter.dispatch` called
  from a node lists nothing; from a macro, only the implementation for
  the adapter.
- Both builds support versioned models, each version a node such as
  `model.p.m.v2`; snapshots declared in YAML; functions (`function('f')`,
  as `function.p.f`), on DuckDB too; and `dbt.ref` and `dbt.source` in a
  Python model.  The scan reads each since plan 018.

### How the server runs it

- The command: made from the project's Dbt (`config::Dbt`), the server
  command read once, as every run of dbt is: the server command with its
  `lsp` word replaced by `parse`, every word before it kept (`uvx dbt`),
  then from after it only
  `--profiles-dir`, `--profile`, `--target`, `--vars` and
  `--packages-install-path`, in either form; always `--project-dir <root>`,
  `--target-path` and `--log-path` under the cache, `--write-json`,
  `--no-version-check`, `--no-send-anonymous-usage-stats`,
  `--no-manage-state`, `--quiet` and `--log-format json`: dbt runs as the
  user runs it, with dbt's own flags for its statistics and its version
  check, and nothing taken from it, network or files (the owner,
  2026-11-18).  A server command with no `lsp` word gets no parse.  The
  working directory is the root's on the folder's route and a document's,
  the server's when given (see "Root"), and the environment the server's.
  So a launcher (`uvx dbt`) runs as the user's does, and fetches what its
  cache lacks.  dbt 2 adds `dbt_internal_packages/` to a project's
  `.gitignore` that lacks it, a project with no `.gitignore` aside: a
  background parse may change a tracked `.gitignore`, which the owner
  accepted on 2026-11-19.
- The child: no input; both outputs in `parse.log` in the cache, never on
  the client's channel; a failure names that file and gives dbt's first
  error's message from its JSON log, else the first line that is none of
  its events, as a launcher's own.  A thread waits on it and
  reports to the event loop; it is killed past two minutes, and at `exit`
  or when the client closes.  Only the direct child is killed: a launcher's
  grandchild may outlive it.
- When: at the first `didOpen` in the project, at each change of the
  client's that may change the graph (a save, a watched file, a file
  created, renamed or deleted, but under dbt's `target/` and `logs/`),
  and at a request that finds the index stale: a `nodeInfo`, a hover, the
  call hierarchy, or a definition or references, each answered from the
  last index meanwhile; an inlay hint request only when there is no index
  yet.  Such a request first looks at the files the scan holds, by their
  metadata: one that may hold a resource (not an editor's auto-save
  `#x.sql#` or backup `x.sql~`) written, added or removed since the last
  parse started, or a package come or gone, marks the index stale too,
  with no change told, as after Claude Code's `Edit` of a file it had not
  opened, which it opens without saving (the Workspace's own refresh).  A
  parse starts when every package is local or `package-lock.yml` is no
  older than the file naming the packages (an empty or comment-only file
  names none), and before `shutdown`; one at a time, and the next at the
  end of one when a change came during it.  A change found on disk waits
  for a request: the end of a parse and a notification never look, so
  that a run of dbt that wrote such a file would start no other by
  itself.  A reason a parse cannot start, or a failure, is logged once
  with `window/logMessage`, and never after `shutdown`.  With a dbt that
  cannot run, the parse fails: `nodeInfo` then gives its failure.
- The packages (`packages::refused`): a parse is refused, and why logged,
  when dbt would install packages first, which writes `package-lock.yml`,
  rewrites the install path, deleting what it holds, untracked files
  included, and reaches the network for a package from it.  dbt does so
  when a package is missing from the install path, whatever the lock
  says.  So a parse needs: each `local:` package of `packages.yml` and
  `dependencies.yml`, the project's and, in turn, its local packages',
  each read from the root as dbt does, at `<install path>/<its project's
  name>`, a `dbt_project.yml` there (in a directory or the link `dbt
  deps` makes), as dbt checks; each package from the network (`package`,
  `git`, `tarball`, `private`) named there in a `package-lock.yml` no
  older than the project's files; each package of the lock, which names
  them all, local ones included, installed the same way; and each of
  those files, with `dbt_project.yml`, read as dbt reads it: a byte order
  mark aside, refused when yaml-rust2 does not read it (a tab after a key,
  which dbt reads) or when a file naming packages repeats a key or holds
  an alias or a merge key (dbt takes the last value, and through them).
  One risk is uncovered since plan 056: a `package-lock.yml` as recent as
  the files naming the packages but stale in content passes the guard;
  dbt resolves anew, reaches the hub or git, and rewrites the install
  path, deleting what it holds.  Until plan 056 a read-only mount of the
  project stopped that install on Linux, and a refusal of every package
  from the network stopped it elsewhere; dbt's own `sha1_hash` in the
  lock would tell such a lock, and no plan reads it yet.  The install
  path is the server command's `--packages-install-path`, else
  `DBT_PACKAGES_INSTALL_PATH`, set before its `lsp` or in the
  environment, else the project's `packages-install-path`, else
  `dbt_packages`.  Probed on both builds: a `local:` package under the
  install path in a directory not named after its project, one beside the
  project not yet installed, a local package's own `local:` package, a
  locked hub package missing after `dbt clean`, a hub package a local
  package names with no lock, and a `packages.yml` behind a byte order
  mark each made dbt rewrite the install path, the packages it held
  included, or try the hub.
- The cache: `$XDG_CACHE_HOME/dbt-lsp-plus/<hash of the root and the
  command>/<pid>/`, else under `~/.cache`, an empty or relative value being
  ignored; removed at exit; those beside it that no parse wrote for a day,
  as a server or a select killed by a signal leaves, removed when the next
  server starts or a select runs, a directory with no `parse.log` aged by
  its own time and one whose `run/run.log` is newer kept (asking whether
  a process lives would miss a home shared between hosts);
  the manifest is
  deleted before each run, so that only one this run wrote counts.
- The index: the manifest's nodes, sources, disabled nodes and groups, with
  `relation` (dbt's `relation_name`, null for an analysis) and `tests` (the
  ids attached).  A run that fails part of the way lays its entries over the
  last index.

### Inlay hints

After each `ref` or `source` whose arguments are literals, right after
its closing parenthesis, the relation dbt builds for its node, from the
last parse's index, or `ephemeral` by the node's materialization; none
for a node the lookup does not lead to, a disabled node, or a `ref` that
a keyword argument follows, since its place is found only right after a
literal argument.  Kind Type, padding on the left, the node's unique id
as tooltip.  Computed from the document's text and the index when the
request arrives, from the project's scan too, each call's node the one
its definition leads to; `[]` with no index, no project or no call, and
error -32603 when the computation panics, as for any request (plan 075).
A hint request starts a parse only when there is no index yet, since
clients ask on every edit and scroll; after each parse that gives an
index, a client that declares `workspace.inlayHint.refreshSupport` is
asked to refresh, under an id `dbt-lsp-plus/N`.  The values of variables
are not hinted yet: in dbt 2.0.9 a macro's `var()` takes its caller's
scope and string values are rendered.

### What the client gets

- `workspace/executeCommand` `dbt-lsp-plus.nodeInfo`, announced under
  `executeCommandProvider.commands`: arguments `[uniqueId]` or
  `[{uri, position}]`, a position naming the `ref` or `source` under it,
  or the entry whose `name:` is there,
  else the one node its file defines, but in a YAML file (a properties
  file, or a YAML snapshot's), where the manifest also places the generic
  tests it declares.  A `ref` or a `source` names the node its definition
  leads to (`Lookup::node`), and so for a hover and an inlay hint: for a
  `ref`, the version it names, from the calling file's package first,
  none that the last manifest disables, and none it lacks whose file is
  gone (one whose file remains keeps the node the index holds); for a
  `source`, the table that its declaration in the scan gives, even one
  that the last manifest disables, which is then described as disabled,
  with no inlay hint (see "Known limitations").  The result is the
  manifest's node,
  with `summary`, the text of the summary a hover shows first (its lines
  apart by a plain line break), and `children`, the unique ids, sorted,
  of the enabled nodes of the last manifest whose `depends_on.nodes`
  names it, every test aside (`ParseIndex::children`; exposures, metrics,
  semantic models, saved queries and unit tests are not in the index),
  null for no node, and error -32803 with the reason until a first parse
  has finished; after that, the last index answers, and a later failure is
  only logged.  A position names the call under it in SQL inside Jinja, and
  in YAML or a Python model outside it.  nodeInfo, the subcommand `node`
  and MCP ask one query of the core, `Workspace::node`.  MCP comes with
  plan 052.
- A hover on a `ref` or `source` argument, an entry's `name:` in YAML, or
  at position 0:0 (line 1, column 1) of a file that defines one node,
  which stands for that node as nodeInfo's position does, a YAML file
  aside (a properties file, or a YAML snapshot's), resolved from the
  document's text when the request arrives, is the node's summary and
  facts, in markdown or plain text as the client's first format says; on
  a doc, its text; on a generic test, its signature.  Elsewhere, null,
  but on a node before any index (below).  The summary is one
  line of what dbt builds and where, then one of what is set beyond the
  defaults: group and owner, access when not protected, an enforced
  contract, the version, tags, disabled, the number of tests.  The facts
  follow, each on a line of its own, hard in markdown: the unique id, with
  `, properties <file>` when a properties file patches the node, or a
  source's own file: from the root, or, for a package's node,
  `<package>://` and from the package's directory, as dbt writes
  `patch_path`; then, when
  any column is declared, `columns ` and each one's name and declared
  `data_type`, by name, as the manifest is read.  Each value of the facts
  (the id, the file, each column's name and type) is a code span, its
  fence longer than any run of backticks inside, and a space of padding
  when it begins or ends with a backtick or a space: markdown shows a
  type such as `array<string>` whole rather than as a tag, and the raw
  text, which Claude Code hands its agent unrendered, keeps every byte of
  a path such as `models/marts/_orders.yml`.  A line break inside a
  value, as in a type written as a YAML block, is given as a space, as
  markdown shows it, so that each fact stays one line.  Plain text gets
  the same spans.  nodeInfo's `summary` holds the summary alone.
- A hover that names a node before any index exists (a `ref` or a `source`, an
  entry's `name:`, or 0:0 of a file but a YAML file whose scan holds
  one model, seed, snapshot, analysis or singular test of the root
  package) is error
  -32803 with nodeInfo's reason, word for word: `dbt parse has not
  finished: it is running`, its failure, or `it has not run, or left no
  manifest`.  The hover asks for the index first, as nodeInfo does.  A position that names nothing, a macro call's
  argument included, stays null; once an index exists, a node it lacks is
  null too, even while a parse runs.  An agent reads the error's message
  (Claude Code shows `Error performing hover: ...: <message>`), where a
  null reads "not indexed"; lsp-mode's eldoc ignores a hover's error.

## The parse's graph, problems and watcher

The server, from its parse:

- takes the graph's edges between nodes from the last parse's manifest,
  `depends_on.nodes` of its enabled nodes (`ParseIndex::dependencies`,
  which a node's `children` read too) of the kinds that `dbt list` lists,
  `*` and `source:*`, and starts a parse for a request of the call
  hierarchy that finds the index stale; a node that the scan missed is an
  item at the start of its file; a failed parse's manifest lacks the nodes
  deleted since, which the index keeps for `nodeInfo` but the graph drops;
  the edges to and from macros
  stay the scan's, as the manifest's `depends_on.macros` holds every macro
  rendering reached, dbt's own included, and none through a dispatch;
- publishes the parse's errors and warnings: each line of the JSON log of
  level `error` or `warn`, placed by the last line of its message,
  `  --> PATH[:LINE:COLUMN]`, lines and columns from 1, columns in code
  points; the path in the root if it is there, else in dbt's working
  directory (see "Root"), else in the one package that holds it,
  as dbt names a YAML loader's error from its working directory, with
  `..` from a sibling, resolved as written, and a package's file from its
  package; `dbt_project.yml` too, when the scan cannot read it; a range of no
  width there, the file's start when dbt gives no position; severity 1 or
  2, `source` `dbt parse`, `code` dbt's bare number, the message without
  its `[error] ` or `[warning] ` prefix.  A
  problem in no such file is logged once.  A parse that ran to
  `CommandCompleted` (Q039) replaces the last one's set, and publishes
  again each file that has problems, before or now, but an unchanged
  document with edits unsaved, whose ranges the client has moved; a
  parse that ends before keeps it.  They are published with the last
  run's failures; an edit that drops a failure leaves their ranges to the
  client until a parse publishes the file again, and
  so does a new run for a document with edits unsaved and problems of the
  parse.  A document counts as unsaved by its text against the file's,
  line ends aside, as Emacs sends a CRLF file with LF;
- registers, after `initialized` and for a client that allows dynamic
  registration of watched files, a watcher of `**/*`
  (`dbt-lsp-plus-file-watcher`), so that an edit outside the editor
  parses again; an event whose every change lies in dbt's `target/` or
  `logs/`, which the user's runs rewrite, starts none.

## The CLI

`doctor`, `select` and `node` (`src/cli.rs`, `docs/reference/cli.md`)
answer an agent in a shell, without an LSP client.  A subcommand is the command
line's first word and nothing else, so that every command line that
starts the server, `--` first or an option first, keeps its meaning; a
second binary would be one more file to install, put on a harness's
`PATH` and allowlist.

- **One resolution of the dbt.**  Each finds the project on the server's
  routes, through `locate::project`: the server command's
  `--project-dir` (`locate::project_dir`), else the path as a client's
  folder, else as a document's (`locate::project_of`), or why none, each
  subcommand's advice at its end.  The server keeps its own coding of
  those routes, at its own moments: the given one at its start, a folder
  at `initialize`, a document at its first `didOpen`.  Server and CLI
  then find the dbt of the project by one function, `locate::dbt_of`:
  the command as written when given, else `locate::dbt`'s from the
  command anchored in the current directory (`config::anchored`).  One
  divergence: a given `--project-dir` without `dbt_project.yml` is no
  project to the subcommands, where the server takes the directory all
  the same, and its parse fails there.
- **doctor runs what the server runs.**  git once and `lsp --help`; it
  probes, as `locate::dbt` probes a candidate, the dbt that it leaves
  unprobed (the one on `PATH`, the user's own first
  word, the command as written when given), so that it says of every dbt
  whether it answers as a dbt 2 does.  Its `packages` is
  `packages::refused`: why the session's first parse would be refused,
  alike on every route.  Without a project it runs nothing, as the server
  stays quiet.
- **select is `dbt-lsp-plus.nodes` from a shell.**  The same guard, the
  same run (`commands::nodes_job`, `config::Dbt::run`, `commands::start`
  with the parse's deadline), in a run directory of
  its own under the user's cache, removed once the answer is read; its
  answer is `listed`'s, as a client gets it.
- **node is nodeInfo from a shell.**  The same query of the core,
  `Workspace::node`, on a Workspace of the CLI's own: its project and dbt
  found as above, an id's from the current directory, a place's from its
  file; its index the manifest kept in `kept/`, beside the project's run
  directories, under the stamp of the files on disk (`cache::disk`) when
  its parse started (`config::kept`), else a parse of its own, the guard
  deciding; its manifest kept by a rename when it does not
  fail (`config::keep`), and the Workspace stopped once it answers, so
  that only the kept manifest outlives the run.  Two runs at once each
  parse apart, with no lock, which a killed run would leave behind; one
  may remove the other's newer manifest, and the next parses again.  Its
  answer is `workspace::reply`'s: the node, or `error` with nodeInfo's
  reasons, the parse's failure in place of `dbt parse has not finished`.
- **The answers' form.**  One line of compact JSON, whatever the outcome,
  with `error` when something is missing or failed; nothing on the
  standard error but a usage error, status 2.  No answer has a time or a
  parse's id: doctor and select read no index, and node's is the
  manifest of the files as they lie; nor a `from`, since each has one
  provider.

Left for later: fields, limits and paging for `select`, and a fast path
over the manifest that node keeps, checked against dbt (plan 058);
`lineage` (plan 052); the profile and the target in doctor, which need
`dbt debug` and the warehouse.

## Failure modes

| Situation | Behaviour |
|-----------|-----------|
| No `--project-dir`, and neither the client's folder nor a document opened yet lies in a project | Null to every request of the server's own methods and commands, -32601 to any other, nothing run, nothing said (see "Root") |
| A `.venv/bin/dbt` that cannot run or is not a dbt 2 | Skipped and named in the line; the next place, else `PATH` (see "Root") |
| A repository whose `.venv/bin/dbt` is not to be trusted | Run as soon as a file of its project opens, on the folder's route and a document's: its probe, then the parse.  A server command with a path to dbt, or `--project-dir`, runs what it names |
| A method or a command the server does not answer | Error -32601 `dbt-lsp-plus does not answer NAME`, at once |
| The root's `dbt_project.yml` no longer reads: removed, not YAML, or with no `name` | A definition or references on a target, workspace symbols and the call hierarchy fail with -32803 `no dbt project at ROOT: ...`; with no target, a definition is null and references `[]`; once a parse has given an index, a hover and a nodeInfo at a `ref`, a `source` or an entry's `name:` fail with that reason too, and inlay hints give none (see "What a request gets") |
| A YAML file is not valid | Its sources are left out |
| A frame that `lsp-server` cannot read: a header line without CRLF, a header without exactly `: `, no `Content-Length`, a body that is not UTF-8 JSON, JSON of no message's shape | The session ends as when the client closes its input: a parse or a run killed, status 1; the last answers sent may be cut |
| A request whose id is neither a string nor a 32-bit integer | Read as a notification, never answered; before `initialize`, dropped |
| A request before `initialize` | -32002; a notification is dropped, and `exit` ends the server, status 1 |
| A message between `initialize`'s answer and `initialized` | The session ends, status 1 |
| The client goes away during the handshake | The server panics, status 101; nothing had started |
| `exit` | A parse or a run under way is killed.  The server exits 0 after `shutdown`, else 1 |
| `dbt parse` fails, or cannot start | The index keeps what it had, laid over by what a failed run still wrote; the reason is logged once, and given by `nodeInfo` and a hover on a node until a first index exists |

The 64 MiB limit went: a stdio client, which started the server, is
trusted.

## Known limitations

- The commands for clients run by dbt's CLI as the user runs it: a compile of a model with `run_query` under `execute`, an
  introspective macro or a Python model may reach the warehouse, and dbt
  may download a driver.  A compile is refused where a package may have
  on-run hooks, or where a package's `dbt_project.yml` does not read.  A file of a package, or whose path from the root
  holds a space or a comma, does not compile.
- `docs-paths` is ignored: docs blocks are read from every resource path.
- JSON-RPC batches are not read: a batch ends the session.  LSP clients
  do not send batches.
- A backslash-escaped quote in SQL, dollar-quoted strings, and `#` or `//`
  comments holding a quote can mislead the CTE scan.  So can a named window:
  in `window w1 as (...), w2 as (...)`, `w2` reads as a CTE.
- A backslash-escaped quote inside a Jinja string, as in `'it\'s'`, ends the
  string for the scan; an odd number of them hides the blocks after it.
- SQL files under `tests/fixtures`, and those under `tests/generic` without a
  test block, count as singular tests.
- A dispatch names the implementations of its namespace only: the root
  project's own, which dbt tries first, and a `dispatch:` search order in
  `dbt_project.yml` are not followed.
- Custom materializations are in the outline but not in workspace symbols.
- A new file not saved yet is not part of the project.
- A version that YAML reads as a number other than as written, such as
  `v: 1e3`, which dbt takes as 1000, keeps its written form in the scan:
  its file `<name>_v1e3` is the version there, where dbt finds none.
- The scan lists files that the root's or a package's `.dbtignore`
  matches, which dbt 2 leaves out: they get workspace symbols, call
  hierarchy items and edges that dbt does not have, and can be a second
  candidate of a definition before the first parse.
- A hook's calls, in `dbt_project.yml` or a properties file's `config:`,
  are references, but no edge of the nodes that the hook configures: the
  call hierarchy and the corpus do not attribute them.
- A project's macro named `function` replaces dbt's `function()`, as one
  named `ref` replaces `ref()`: dbt then records the macro and no
  function, while the scan still draws the edge to the function.
- A file that the project reaches through a link, opened by its real path
  outside the project, is not matched to its resource.
- A package of the root installed outside it through a `.env` in the
  root that sets `DBT_PACKAGES_INSTALL_PATH`, or through `env_var` in its
  `packages-install-path`, is taken for another project: on its files,
  `prepareCallHierarchy` fails at once with
  `<file> lies in the dbt project at <package>, ...`, and so do a hover,
  a definition and references.  A method set to `dbt` gets no such
  reason (`Proxy::outside` gives none), but its nothing, a hover null.
  `locate::elsewhere` reads the install path from the flag, the variable
  in the server command or the server's environment, and the key as
  written (plan 056).
- In YAML, an apostrophe in a plain scalar, as in `description: It's the
  customers`, can unmask a later comment: a call commented out after it
  counts as a reference, and a rename edits it.  A `#` that follows no
  space, as in `tags: a#b` or a Jinja comment `{# #}`, hides the rest of
  its line as a YAML comment would.
- `selectors.yml` and selectors on the command line name a source as
  `source:s.t`; a rename leaves them on the old name.
- Killing a parse kills `dbt` itself, or the launcher before it (`uvx`,
  `nix run`), whose child may then run on until it ends.
- A column or an alias spelt like a CTE counts as one of its uses, for its
  highlights and its rename.
- The last run's target path is the server's `DBT_TARGET_PATH`, not the
  shell's where the user ran dbt, and a run's `--target-path` is not seen.
  A failure stays until a new run, an edit of its file, or a save; on a
  file with problems of the parse, until the next parse publishes it.  Of
  two tests of one name on one entry, a failure goes to the first.  A
  local package linked into `dbt_packages` has its failures under the
  link's path, which an edit of the real file does not clear.
- The outline shows a versioned model's columns, those of its versions
  included, as the model's.
- With both `run_results.json` and `sources.json` present, each change of
  the manifest is parsed once per watcher.  A source written as two
  blocks of one name puts the second block's stale tables at the first
  block's name.  A table whose freshness query failed under 2.0.9 shows
  nothing, and a stale warning shown before goes.
- In a Python model or YAML, a `#` comment between a `ref`'s last
  argument and its `)` hides its hint.
- Navigation reads that flag as written.  It reads no explicit tag:
  `!!str false` puts the root first, where dbt keeps the calling node's
  package first, and `!!bool "false"` does not, where dbt does.  It
  neither follows an alias nor renders Jinja: dbt 2.0.9 and dbt-oss 2.0.11
  put the root first for `*no` naming a `false`, for `'{{ False }}'`, and
  for a comparison of an `env_var` that gives false.
- A `ref` or a `source` inside a macro resolves in dbt in the package of
  the node that calls the macro, not in the macro's own; the server, which
  does not know the caller, resolves it in the macro's package.  It shows
  in the navigation of a `source()` in a macro file, and in the call
  hierarchy's edges from a macro, which always come from the scan.  So
  does an unqualified macro call inside a package's macro, by the owner's
  choice: navigation, references and the graph look it up in the macro's
  package, then the root.
- Before the first parse, a generic test's arguments (`column_name:`,
  `to:` aside) and a `config:` block lead nowhere.
- Where one package has two files of a model's name, one of them disabled,
  a definition before the first parse leads to both.
- A source table that the root disables leads a definition, a hover and
  nodeInfo to its own declaration, described as disabled and with no
  inlay hint, since navigation reads a source from the scan alone, while
  the call hierarchy, once a parse exists, skips it for another package's
  declaration of the same name.
- A hook given in a string inside a tag, such as `{{ config(post_hook="{{
  m() }}") }}`, is not read: its macro calls are no references.
- A `doc` in a project that sets `docs-paths` to a directory outside its
  resource paths finds no docs block (see `docs-paths` above).
- A hover or a definition in a large properties file parses it several
  times, since no YAML tree is kept: about 0.2 s per request on 2.8 MB of
  YAML, in a release build, on the loop that answers every message.
- The kept scan misses an edit on disk that keeps the file's size within
  one tick of the file system's clock after the scan read it: its
  metadata is the same.  The next change of the file shows it.
- Each request that needs the project walks it, reads the metadata of
  every file and each package's `dbt_project.yml`, and the root's
  `packages.yml` and `dependencies.yml`: most of the 30 to
  40 ms at 6,000 models.  A request that asks for the index walks it
  once more when the index is fresh and a parse has started, for the
  stamp of the files on disk (`Workspace::parse` asked to look); and each
  parse's start walks it once more, for its stamp (`cache::disk`).
- The references of a macro that each of 6,000 synthetic models calls,
  6,001 locations and 1.5 MB of JSON, take about 86 ms: the walk above,
  about 25 ms to find and place them, and the rest to encode the answer
  and for the client to read it.  At 3,000 models, 41 ms.
- A request that comes while the first read of every file is under way
  reads them all itself, beside that read: 0.4 to 0.55 s and 0.72 to
  1.06 s on 3,000 and 6,000 synthetic models, where a scan of its own
  took 0.30 and 0.53 s without the cache, which derived less.  The server
  then holds up to 115 and 208 MB.  Once that read is done, the first
  request takes 36 and 77 ms.
- A file that cannot be read, for its permissions or an error of the
  file system, makes each request that needs the project assemble it
  again: about 35 ms more at 6,000 models, and the call hierarchy's graph
  made again, as after a save (0.19 s against 0.035 s warm).
- The outline of a YAML file that yaml-rust2 does not parse is empty: one
  in the middle of an edit, but also some that dbt reads, being laxer about
  indentation inside flow collections and quoted scalars, such as a flow
  sequence whose `]` sits at its key's column, a quoted description
  continued at its key's column, or a tab after a colon.  The scan of
  sources reads no table from such a file either.  The outline shows no
  tests nested under a resource, model versions, macro arguments, nor a
  semantic model's entities, dimensions and measures.
- A resource path named `target` or `logs` (dbt 2.0.9 accepts
  `model-paths: ["logs"]`) starts no parse on an outside change of its
  files, until the next save; so does every file below the directory that
  `target` or `logs` links to, when it holds the project.
- The corpus's `divergences.txt` lists each divergence from dbt that it
  shows, with its reason (see "Conformance").  Beyond those above: a
  package's model that the root project disables (`+enabled: false`)
  still resolves, ahead of the root's, in navigation and the call
  hierarchy before the first parse; a `function()` inside a package's
  macro resolves in the macro's package, as a `ref` there does, where dbt
  resolves it in the calling node's; the graph names every implementation
  of a dispatch, where dbt lists the adapter's alone from a macro, and
  nothing from a node; dbt 2 ignores a source's `overrides`, which the
  scan follows, so a hover, nodeInfo and an inlay hint on such a source
  describe the package's table and its relation, which dbt 2 does not
  build there.
- A problem that dbt places at a path both the root and a package hold
  goes to the root's file.  With a client that honours the server's
  watcher, a save parses twice: the save, then the watcher's event for
  the same file.  The probe of `lsp` adds one start of dbt, 12 to 22 ms
  on both builds.  The guard judges a lock recent by its time: a comment
  added to `packages.yml` after it refuses a parse until `dbt deps`, though
  dbt's own check, a hash of the packages, would let it through; a lock
  entry no longer named, which dbt leaves, is refused too.  A packages
  file, the lock or `dbt_project.yml` that yaml-rust2 cannot read, as one
  with a tab after a key, which dbt reads, refuses the parse.  A
  change to `packages.yml` in the moments after the guard read it reaches
  dbt unchecked.

## Conformance

The corpus, `tests/fixtures/conformance/`, holds one small dbt project per
rule of dbt's lookup that the proxy follows or misses.  Each is parsed by a
real dbt as the proxy parses (`config::parse_command`), on a scratch copy,
offline (the run wrapped in `unshare -rn` from outside, as CI runs it; an
empty environment but `PATH` and a scratch home, the proxy's deadline),
and its manifest is compared with the
library's answers, each named by a lookup:

- `scan`: what dbt knows in the project's packages (nodes but generic
  tests, sources, functions, macros, docs blocks) against the scan's
  entities and source tables; and a dependency that no call the scan
  reads names;
- `location`: the file of each, against `original_file_path`;
- `navigation`: each `ref`, `source`, `function` and macro call of a node, as
  navigation resolves it once a parse exists, the case's manifest as its
  index; and in a properties file each entry's `name:` and each `doc()` in
  it, against the dependency of the
  call's name, the node or function that dbt patched from that file (`patch_path`),
  and the docs block of the doc's name whose text dbt rendered in the
  description that holds the call, a column's or the node's;
- `graph`: each call of a node or a macro, by the edges whose sites hold
  it, in the graph built with the case's manifest as its index, as the
  proxy builds it once a parse exists, against the dependencies (nodes,
  and the project's macros) that bear the call's name, a dispatch's being
  its `*__name` implementations.

A node is compared on its own calls and on the `ref`, `source` and macro
calls of each macro it reaches through the graph by plain calls, as
`<node> via <macro>`, since dbt resolves those for the node; but not through a
dispatch: dbt renders the one implementation it picks, which the corpus
cannot tell, so no implementation's `ref` or `source` is compared, though
their calls still count for what the node names.  A macro is compared on
its own calls of macros; a node that dbt dropped, against nothing.  A
case may expect dbt to fail, in a file `expected-error`, and its manifest
is compared all the same; dbt must leave the copy as it was.  `divergences.txt` lists every divergence,
what each side answered and why: one it lacks, or an entry that no longer
diverges, as when an answer changes, fails the corpus.  dbt 2.0.9 and
dbt-oss 2.0.11 gave the same divergences on every case.  Every call that
navigation resolves must also be among the references of what it leads
to, which no registry excuses.

A case keeps to what the comparison can attribute: one call per node and
target, a versioned `ref` by its version, every call rendered at parse,
no hook, and docs blocks of distinct texts.  A version is named by its
number, and by none when it is the latest, as the manifest says.  Not
compared: a generic test's name, a unit test's `model:`, a `ref` or
`source` in YAML, whose nodes dbt generates, but a snapshot's
`relation:`, and a `doc()` in a source's description.

## Tests

- Unit tests for each pure module: positions and incremental
  changes, the scan of SQL and Jinja, YAML places, the project, the kept
  scan, the graph, the probe, and the shape of each answer.
- Integration tests run the binary with `fake-dbt`, a scripted dbt
  that refuses `lsp` as dbt-oss 2.0.11 does and plays `parse`, `compile`
  and `list` as the script says, logging each process's `argv[0]`,
  arguments and working directory.  They cover:
  - `initialize`: the capabilities and the commands, no `experimental`,
    `initializationOptions` not read; a bad option, a usage error; an
    unknown method or command, -32601, before a project too, a command
    named as a method and the server's notification sent as a request
    among them; a command with no name, -32602; a rename at no source
    and no CTE, null;
  - the outline of unsaved edits in UTF-16, after a character that UTF-16
    and UTF-32 count apart; flat for a client without hierarchy; a closed
    document's edits dropped; the outline of a properties file, flat and
    nested, and none for `dbt_project.yml`;
  - workspace symbols and the call hierarchy, the graph from the parse's
    manifest, a hierarchy request starting the parse;
  - the kept scan between two requests: an edit on disk that nothing
    announced, a document changed, closed unsaved and opened again at
    version 1, a file created then deleted, the project's name and model
    paths, an open `dbt_project.yml` renaming the project once saved, a
    package's file replaced with its size and time kept;
  - the root and the routes: `--project-dir` first and as spelt, with
    its one line, on the standard error too; the client's folder without
    it; the working directory naming nothing; without a project, null to
    every request of the server's, -32601 to another, no other frame,
    nothing run and nothing on the standard error, with stubs of `dbt`
    and git alone on `PATH`; a document's project, as Claude Code opens
    it after a file outside every project, with one line; the parse and
    a compile in a document's project; the walk up from a document over
    scratch layouts, and the lookup of dbt in scratch repositories, with
    a scratch home and no global git configuration;
  - references and renames of sources, files the scan does not hold, the
    client's URI of an open document; navigation at each kind of target,
    dbt's own test null, a versioned `ref`, functions, a Python model's
    `ref`, a name in any file; a CTE renamed in YAML, not highlighted;
  - node information and the hover: the parse's flags and cache, dbt's
    output kept off the client's channel and its standard error in
    `parse.log`, one parse at a time and one more after a change during
    it, none after `shutdown`, nor for a watched change of dbt's own
    output, through a link out of the project too, nor with packages
    that need the network, the last index answering when they refuse a
    parse; a parse killed at `exit`; a node by id or by position; the
    summary and the facts on a hover of a ref and at a file's start, in
    the client's format; a hover on a node before any index failing with
    nodeInfo's reason;
  - the parse's problems as diagnostics, placed and cleared, and the last
    run's failures and stale sources where written; the URI published
    under before cleared when the client opens the file under another;
  - inlay hints: a first parse from a hint request, none more once an
    index exists; the refresh asked of a client that can;
  - the guard on packages, alike on every route; the commands for
    clients by dbt's CLI, one run at a time, refused where a package may
    have on-run hooks, and with a server command without `lsp`.
- Two runs of the integration tests in one checkout share Cargo's
  temporary directory, so they take turns: a process's first scratch
  directory waits for a lock there, held until the process ends.
- CI runs the same checks on GitHub Actions, and the corpus and the live
  test on dbt-oss 2.0.11, and on dbt 2.0.9 once the owner allows it;
  each week, the same on the latest stable dbt-oss, and on the latest
  proprietary build when allowed: see
  `docs/reference/release-pipeline.md`.
- The conformance corpus, opt-in as the live test (see "Conformance"),
  built with the network, `cargo test --locked -j 4 --test proxy
  --no-run`, then run with none: `unshare -rn make conformance
  DBT_LSP_PLUS_TEST_DBT=/path/to/dbt`.
- One opt-in live test, `with_the_real_dbt`, runs the server with the
  real dbt, either build, on a copy of the fixture, from an empty
  environment with a scratch home, built as the corpus is, then run with
  none: `unshare -rn make live DBT_LSP_PLUS_TEST_DBT=/path/to/dbt`.  It checks the outline of a model and of a
  properties file, folding, highlights, selection ranges, a hover at a
  model's start before the parse has ended, the summary or the reason,
  never null; navigation in a properties file and of a ref in SQL; the
  last run's failure, a warning of `dbt parse` possibly beside it; the
  hierarchy from the manifest, `order_summary`'s upstream through its
  variable; node information, the summary alone on a hover, the hints;
  the facts of a hover at a model's start; a model written outside the
  editor with an unknown `ref` shown as dbt1048 from the watcher's
  event, then cleared; the commands by dbt's CLI; and the rename of a
  source table, after which dbt lists the new table and the model still
  compiles to the old relation.  A second one finds the project and its
  dbt without `--project-dir`, from a document, from the folder, and
  from a shell.

## What it would take to finish

In the order of the feature list agreed with the dbt-mode session:

1. Source references and rename (A4): done in plan 002.
2. Resolved configuration and the hover summary (A5, A20): done in plan
   004.
3. Folding ranges (A7), with highlights, selection ranges and CTE rename:
   done in plan 003.
4. A cache of the scan: done in plan 016.
5. YAML outline (A1): done in plan 005.
6. Restart of a crashed server, replaying `initialize`, `initialized` and
   the open documents: done in plan 010, gone with `dbt lsp` in plan 075.
7. Local packages before their install, versioned models, Python models,
   YAML snapshots and functions in the scan: done in plan 018.
8. A run in an editor, then monorepos: one root per session today.

## History: `dbt lsp` behind the proxy

From plan 001 to plan 081, dbt-lsp-plus was a proxy in front of `dbt
lsp`, the language server of dbt Labs' build of dbt 2, observed on 2.0.9:

- it started `dbt lsp` with the server command and forwarded every
  message byte for byte, but the requests of the features the server did
  not announce, which it answered itself;
- it merged its answers with dbt lsp's (plan 042): a hover's summary
  above dbt lsp's text, its hints after dbt lsp's, the union of the
  locations of a definition or references, with deadlines of 300 ms and
  2 s;
- it listed the graph by `dbt.listNodes`, waited for dbt lsp's
  compilations before its parse, said a compilation that compiled nothing
  or had no end, and restarted a crashed `dbt lsp` at most three times in
  ten minutes, replaying the session;
- with a dbt without `lsp`, dbt-oss, or `--backend never`, it answered
  alone, as it now always does;
- `dbt-lsp-plus.show` went to `dbt.show`, and the modes (plans 019 and
  020), `plus`, `dbt` and `merge` by `--mode`, `--default-mode` and
  `initializationOptions.dbtLspPlus`, chose between the proxy, dbt lsp
  and both, the proxy's kinds of target named in
  `experimental.dbtLspPlus`.

The owner decided on 2026-11-22 that the server stands alone: dbt lsp
needs dbt Labs' build, answers as nothing documents, and costs most of
the code; what it alone gave (completion, formatting, its compile
diagnostics, column features) a user may still get by running it beside
dbt-lsp-plus (see `docs/how-to/troubleshoot.md`).  Plan 075 removed it;
plan 085 removed the modes.  The full record of what dbt lsp 2.0.9 was
observed to do, its messages, compilations, listings and restarts, is in
`git show 61cdd67:docs/explanation/design.md`.
