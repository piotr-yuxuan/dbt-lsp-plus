# Features

What dbt-lsp-plus adds to `dbt lsp`, the language server of dbt 2, by LSP
method:

- `textDocument/documentSymbol`: the CTEs of a model and its Jinja blocks
  (macro, test, snapshot, materialization, docs, set, by their name; if,
  for and call, by their statement), nested by extent, as
  the buffer stands, unsaved edits included; in a YAML properties file, its
  models, seeds, snapshots, analyses, macros, functions, data tests,
  sources and their tables, columns, exposures, metrics, semantic models,
  saved queries, unit tests and groups;
- `workspace/symbol`: every model, seed, snapshot, analysis, singular test,
  macro, generic test, function, source table and docs block, installed
  and local packages included, Python models and snapshots declared in
  YAML too, each version of a model as `orders.v2`; each one's
  `containerName` its unique id, such as `model.shop.orders`, which
  begins with its resource type and package;
- the call hierarchy (`textDocument/prepareCallHierarchy`,
  `callHierarchy/incomingCalls`, `callHierarchy/outgoingCalls`): what a
  model, source, macro or function is used by, and what it
  uses, macros and functions included, with the place of each call.  The dependencies of
  the nodes are those of `dbt.listNodes` once the server has compiled, so
  that `ref(var('x'))` resolves; the edges of macros come from the files;
- references and renames of sources (`textDocument/references`,
  `textDocument/rename`), which dbt lsp answers with nothing:
  every `source()` call, in SQL, YAML, `dbt_project.yml` and Python models,
  and the declarations.  A rename keeps the warehouse relation, inserting
  `identifier:` or `schema:` with the old name, and is refused rather than
  applied in part;
- go to definition (`textDocument/definition`) from a properties file,
  where dbt lsp answers nothing: a
  `ref` to its model, a `source` to its table's entry, a `doc` to its docs
  block, a model's or a function's `name:` to its file, a generic test's
  name to its test block, by dbt 2.0.9's lookup across packages, versions
  included; and from a `source()` in SQL; and, with references
  (`textDocument/references`), from a `ref` in SQL or a Python model, a
  macro call and a `function()`, where dbt lsp misses a package's `ref`,
  a version and the references in other files; hover
  (`textDocument/hover`) there shows the docs text, the test's signature,
  the node's summary;
- the last run's failures as diagnostics
  (`textDocument/publishDiagnostics`), beside dbt lsp's own: after a
  `dbt build` from anywhere, a model's error at its file, a test's
  failure at its name in YAML; after `dbt source freshness`, each stale
  source table at its name;
- inlay hints (`textDocument/inlayHint`) after the server's: the relation
  dbt builds, after each `ref` and `source`;
- `dbt-lsp-plus.compile`, `dbt-lsp-plus.show` and `dbt-lsp-plus.nodes`
  (`workspace/executeCommand`), commands for clients that do what dbt
  lsp's `dbt.compileFile`, `dbt.show` and `dbt.listNodes` do, in their
  shapes: by them when dbt lsp runs; without it, `compile` and `nodes` by
  dbt's CLI, cut off the network where the system can (Linux), and
  `show` not at all, since `dbt show` reaches the network;
- `dbt-lsp-plus.nodeInfo` (`workspace/executeCommand`), a command that
  gives a node's resolved
  configuration, relation and summary from the user's own `dbt parse`, and
  that summary first in the server's hover on a `ref` or a `source`, and
  at line 1, column 1 (position 0:0) of a file that defines one node, a
  YAML file aside (a properties file, or a YAML snapshot's),
  followed by the node's facts, a line each, each value a code span: its
  unique id and properties file, then its declared columns with their
  types.  A line break inside a value is given as a space.  A package's file is written `<package>://<path>`, as dbt writes
  `patch_path`: the path is relative to the package's directory, under
  `dbt_packages/` by default.  Where the proxy
  answers the hover itself (alone, or in `plus`), a hover on a node
  before the first parse has given an index is error -32803 with
  nodeInfo's reason, `dbt parse has not finished: ...`;
- within a document: folding (`textDocument/foldingRange`) of Jinja
  blocks, CTEs, tags and comments; highlights
  (`textDocument/documentHighlight`) of a CTE's uses and of a block's tags;
  selection ranges (`textDocument/selectionRange`) from a word up to the
  file; the rename (`textDocument/rename`) of a CTE in its file, refused
  when a string in a Jinja tag names it, since a macro may render it as
  SQL.

Every other message passes unchanged, byte for byte.  A feature that the
server announces stays the server's, but for the rename of a source or
a CTE, and the definitions and references at the kinds of target that
[Where dbt-lsp-plus answers definitions and
references](../how-to/use-with-an-agent.md#where-dbt-lsp-plus-answers-definitions-and-references)
lists; the initialize result names those, per kind of target and kind of
file, under `experimental.dbtLspPlus`.  In `merge`, the default, with
`dbt lsp` running, a definition or references there goes to `dbt lsp`
too, and its locations that match none of the proxy's (in one file,
the range of one holding the other's) follow the proxy's, in one array.
`dbt lsp` 2.0.9's answers on the fixture add nothing; it reads a
versioned `ref` in SQL without its version, so its location there may
follow the proxy's.  A definition, references, hover or inlay hints
that go to `dbt lsp` in `merge` get its answer when it comes within
300 ms where the proxy adds something (locations, a summary, hints),
within 2 s where it adds nothing; else, and when `dbt lsp` fails, exits
or is not running, the proxy's own: its part, else null, or `[]` for
references and hints.  A hover's text from `dbt lsp` follows the
proxy's part under a line `**dbt lsp**`, or `dbt lsp:` in plain text.
The mode `plus` gives the proxy's answer by itself, at once.
When `dbt lsp` crashes, the proxy starts it again, at most three times in
ten minutes, and gives it the open documents as they stand.  Without
`dbt lsp`, and while it restarts, the proxy keeps serving its own
features; while it restarts and after a fourth crash,
`dbt-lsp-plus.nodeInfo`, the summary and the hints too, from its own
parse.  With a `dbt lsp` that cannot start, that parse runs the same dbt
and fails, and `nodeInfo` gives its failure.

With a dbt that has no `dbt lsp`, such as dbt-oss, the proxy answers alone:
see [Use it with dbt-oss alone](../how-to/use-with-dbt-oss.md).
[The design](../explanation/design.md) says how each answer is made.

## Configuration

dbt-lsp-plus reads no configuration file: what it takes is on its
command line, which ends with the server command, in its environment,
and in the client's `initializationOptions` (see "Modes" below).  Every
option at once, for a project whose profile sits beside it:

```sh
env DBT_PACKAGES_INSTALL_PATH=vendor/dbt_packages \
    DBT_TARGET_PATH=target \
    XDG_CACHE_HOME="$HOME/.cache" \
  dbt-lsp-plus -- \
    dbt lsp \
      --project-dir /path/to/project \
      --profiles-dir /path/to/project \
      --profile shop \
      --target dev \
      --vars '{"start_date": "2024-01-01"}' \
      --packages-install-path vendor/dbt_packages
```

Most setups need far less: `dbt-lsp-plus -- dbt lsp --project-dir
/path/to/project`, or nothing after `dbt-lsp-plus` when the editor starts
it in the project.

### The proxy's arguments

```
dbt-lsp-plus [OPTION...] [--] [SERVER COMMAND...]
```

Options are read from the first word for as long as a word starts with
`-`; the first word that does not begins the server command.  An unknown
option, a missing value or a bad one is a usage error: the reason and
the usage on standard error, exit status 2, nothing started.

- `--mode METHOD=MODE`: who answers the LSP method METHOD, by its full
  name, such as `textDocument/hover`: `plus`, `dbt` or `merge` (see
  "Modes").  Repeatable.
- `--default-mode MODE`: the mode of every method the proxy answers but
  those `--mode` sets.
- `--backend auto|never`: `never` starts no `dbt lsp`, and the proxy
  answers alone, as with dbt-oss.  Default: `auto`.
- `--shadow`: compares the proxy's definitions and references with
  `dbt lsp`'s, in a local log (see "The shadow").  It takes no value.
- `--`: ends the proxy's own arguments; needed only before a server
  command that starts with a dash.
- `-h`, `--help`: prints the usage and exits.
- `-V`, `--version`: prints `dbt-lsp-plus X.Y.Z` and exits.

A value goes in the next word or after `=`, as `--backend=never`.

### Modes

Each method the proxy answers has a mode: `plus`, the proxy alone; `dbt`,
`dbt lsp` alone, its answer passed as it is; `merge`, both.  `ours`,
`plus`'s former name, is still read as `plus`, with one warning a
session.  Set nothing and each method takes its shipped default.  The
table says, for each method and command the proxy takes part in, who
can answer it, its shipped mode, how the answers combine in `merge`, and
how long a request of it in `merge` waits for `dbt lsp`; any other
method is `dbt lsp`'s, and a command takes no mode.  Where the proxy's
answer stands alone, `dbt-lsp-plus's`, `dbt lsp` is not asked in
`merge`, even when the proxy finds nothing: the methods it adds, the
call hierarchy's two others and `dbt-lsp-plus.nodeInfo`.  A rename is
the proxy's at its kinds of target, else `dbt lsp`'s; a command goes to
the first of its providers present, `dbt lsp` when it runs and lists
the command, else dbt's CLI under the cut.  A definition and references
are the proxy's at the kinds of target that `experimental.dbtLspPlus`
lists ([Where dbt-lsp-plus answers definitions and
references](../how-to/use-with-an-agent.md#where-dbt-lsp-plus-answers-definitions-and-references)),
with `dbt lsp`'s other locations after them, and `dbt lsp`'s elsewhere.
A wait is 300 ms where the proxy has something of its own, 2 s where it
has nothing; `no` is no deadline: the proxy's answer at once, or `dbt
lsp`'s however long it takes.

| Method or command | Answered by | Shipped mode | Combined | Waits for dbt lsp |
|---|---|---|---|---|
| `textDocument/documentSymbol` | dbt-lsp-plus, dbt lsp | plus, or dbt where dbt lsp announces it | dbt-lsp-plus's | no |
| `workspace/symbol` | dbt-lsp-plus, dbt lsp | plus, or dbt where dbt lsp announces it | dbt-lsp-plus's | no |
| `textDocument/prepareCallHierarchy` | dbt-lsp-plus, dbt lsp | plus, or dbt where dbt lsp announces it | dbt-lsp-plus's | no |
| `callHierarchy/incomingCalls` | dbt-lsp-plus, dbt lsp | prepareCallHierarchy's | dbt-lsp-plus's | no |
| `callHierarchy/outgoingCalls` | dbt-lsp-plus, dbt lsp | prepareCallHierarchy's | dbt-lsp-plus's | no |
| `textDocument/foldingRange` | dbt-lsp-plus, dbt lsp | plus, or dbt where dbt lsp announces it | dbt-lsp-plus's | no |
| `textDocument/documentHighlight` | dbt-lsp-plus, dbt lsp | plus, or dbt where dbt lsp announces it | dbt-lsp-plus's | no |
| `textDocument/selectionRange` | dbt-lsp-plus, dbt lsp | plus, or dbt where dbt lsp announces it | dbt-lsp-plus's | no |
| `textDocument/hover` | dbt-lsp-plus, dbt lsp | merge | dbt-lsp-plus's text, then dbt lsp's under its name | 300 ms, 2 s with nothing |
| `textDocument/inlayHint` | dbt-lsp-plus, dbt lsp | merge | dbt lsp's, then dbt-lsp-plus's | 300 ms, 2 s with nothing |
| `textDocument/definition` | dbt-lsp-plus, dbt lsp | merge | the union, dbt-lsp-plus's first | 300 ms, 2 s with nothing |
| `textDocument/references` | dbt-lsp-plus, dbt lsp | merge | the union, dbt-lsp-plus's first | 300 ms, 2 s with nothing |
| `textDocument/rename` | dbt-lsp-plus, dbt lsp | merge | dbt-lsp-plus's at its kinds, else dbt lsp's | no |
| `textDocument/publishDiagnostics` | dbt-lsp-plus, dbt lsp | merge | both, per file | no |
| `dbt-lsp-plus.nodeInfo` | dbt-lsp-plus | none | dbt-lsp-plus's | no |
| `dbt-lsp-plus.compile` | dbt lsp, dbt's CLI | none | the first present | no |
| `dbt-lsp-plus.show` | dbt lsp | none | the first present | no |
| `dbt-lsp-plus.nodes` | dbt lsp, dbt's CLI | none | the first present | no |

Besides the command line, a client sets the modes in
`initializationOptions`, which win method by method:

```json
{"dbtLspPlus": {"modes": {"textDocument/hover": "dbt"}, "defaultMode": "merge"}}
```

For the methods the proxy adds (the outline, workspace symbols, the call
hierarchy, folding, highlights and selection ranges), `merge` is the
proxy's answer, as `plus` is, until a later version merges them; `dbt`
leaves the capability as `dbt lsp` announced it, so that a method it
announces as absent is never asked.

In Neovim that is `init_options`, not `settings`: the proxy reads no
`workspace/didChangeConfiguration`, and a change applies when the client
starts again.  A refused key is ignored and shown once in a warning.
`dbt lsp` gets `initializationOptions` without `dbtLspPlus`.  With no
`dbt lsp`, a method set to `dbt` gets nothing, and a warning says so.

### The shadow

With `--shadow`, or `{"dbtLspPlus": {"shadow": true}}` in
`initializationOptions`, the proxy follows each definition and references
request in `dbt`, which it leaves to `dbt lsp` (`--mode
textDocument/definition=dbt --mode textDocument/references=dbt`); with
neither in `dbt`, a warning says it has nothing to follow.  The client
gets `dbt lsp`'s answer
as it came, then the proxy computes its own and compares the two,
location by location.  The session's counts, by method, kind of target,
kind of file and how the two answers stand, answer the command
`dbt-lsp-plus.shadowSummary` (`workspace/executeCommand`), announced only
with the shadow on.  Each request is a line of
`$XDG_CACHE_HOME/dbt-lsp-plus/shadow/<hash of the root>.jsonl`, created
readable by its owner alone; nothing is sent anywhere.  Without `dbt lsp`
the shadow is off, and a warning says so.  A session with the shadow off
leaves the logs as they are: they go once a shadowed session finds them
30 days old, or by hand.  [The design](../explanation/design.md)
says what a line holds and the rule by which a later version reads them.

### The server command

- `SERVER COMMAND...`: the command that runs the language server behind
  the proxy.  Default: `dbt lsp`.  The proxy speaks LSP on its standard
  input and output, and runs this command with the same streams behind
  it.
- The words before `lsp` are how dbt is launched: `dbt`, a path to a dbt
  executable, `uvx dbt`, or `env X=1 dbt`.  Before it spawns the server,
  the proxy runs these words then `lsp --help`, for at most 10 s.  If dbt
  answers that it has no `lsp` subcommand, as dbt-oss does, the proxy
  answers alone, from the files and its own `dbt parse` (see [Use it with
  dbt-oss alone](../how-to/use-with-dbt-oss.md)).
- Without `--project-dir`, when the first word is the bare `dbt` (the
  default included), the proxy runs the project's dbt: the first of the
  project's `.venv/bin/dbt`, the repository's, and, from a linked
  worktree, the main worktree's project's and the main worktree's own,
  that answers `lsp --help` as dbt 2 does, with `lsp` or without; one that
  answers otherwise, as dbt-core 1.x does, or cannot run, is skipped.
  Else the `dbt` on `PATH`.  Any other first word is kept as written.
  The proxy says once, on its standard error and in a `window/logMessage`,
  `dbt-lsp-plus: project <root>, dbt <path> (<where it was found>)`, with
  each place skipped and, when it answers alone, why.  With
  `--project-dir`, the command runs as written.
- A server command without the word `lsp` is run as it is, as a wrapper
  of the user's own: the proxy then runs no `dbt parse`.

### The flags its `dbt parse` keeps

The proxy runs its own `dbt parse` for what it answers from the manifest.
It builds that command from the server command: the same launcher, then
these flags when the server command has them, as `--flag X` or
`--flag=X`.  `dbt lsp` reads them too.

- `--project-dir DIR`: the dbt project.  A relative `DIR` is taken from
  the proxy's working directory.  Default: the client's first workspace
  folder or `rootUri`, when it holds a `dbt_project.yml`; else the
  project of the first file opened in one: the nearest directory above it
  holding a `dbt_project.yml` that lies in no package, installed or local,
  of a project above it.  Before that the proxy runs nothing and answers
  every request with null, and for such a project it runs no `dbt lsp`:
  it answers alone, as with dbt-oss.  The working directory never names
  the project.
- `--profiles-dir DIR`: where `profiles.yml` is.  Default, as dbt's:
  dbt's working directory, then `~/.dbt`.  dbt runs in the project when
  the client's folder or a file named it, so a `profiles.yml` beside
  `dbt_project.yml` is found; with `--project-dir`, in the proxy's
  working directory, not the project's: an editor that starts the proxy
  in a subdirectory of the project needs it.  A relative `DIR`, as a
  relative path to dbt in the server command, is taken from the proxy's
  working directory, wherever dbt runs.
- `--profile NAME`: the profile.  Default: the `profile` of
  `dbt_project.yml`.
- `--target NAME`: the target of the profile.  Default: the profile's
  `target`.
- `--vars YAML`: the project's variables, as a YAML or JSON mapping.
  Default: none.
- `--packages-install-path DIR`: where the packages are installed.
  Default: `DBT_PACKAGES_INSTALL_PATH`, else the
  `packages-install-path` of `dbt_project.yml`, else `dbt_packages`.

Every other flag goes to `dbt lsp` alone.  The parse also gets
`--target-path` and `--log-path` in the proxy's cache, `--write-json`,
`--log-format json`, `--quiet`, `--no-version-check`,
`--no-send-anonymous-usage-stats` and `--no-manage-state`; none of these
can be changed.

### The environment

- `DBT_PACKAGES_INSTALL_PATH`: where the packages are installed, below
  `--packages-install-path` and above `dbt_project.yml`.  Read from the
  proxy's environment, or from a word `DBT_PACKAGES_INSTALL_PATH=DIR`
  before `lsp` in the server command, which wins.  An empty value counts
  as unset.
- `DBT_TARGET_PATH`: where the proxy watches the user's last run
  (`run_results.json`, `sources.json` and `manifest.json`), relative to
  the project.  Default: `target`.
- `XDG_CACHE_HOME`: where the proxy keeps its parses, under
  `dbt-lsp-plus/`.  Default: `$HOME/.cache`, else the system's temporary
  directory.  A relative value counts as unset.  A cache that no parse
  wrote for a day is removed.  The shadow's log is under
  `dbt-lsp-plus/shadow/`, never in the temporary directory: with neither
  variable absolute, there is none.

The server command, `dbt lsp` and the proxy's `dbt parse` inherit the
whole environment.

### Fixed limits

These are not configurable.

- A `dbt parse` is stopped after 120 s.
- The probe of `lsp --help` is stopped after 10 s.
- `workspace/symbol` answers at most 20 000 symbols.
- A message larger than 64 MiB stops the session.
- The shadow's log of a root is renamed `.jsonl.1` once it reaches 1 MB,
  replacing the last: about 2 MB a root.  A line shows at most 20
  locations of each side, and stays under 8 KB but for the session's
  last.  A shadow file that nothing wrote for 30 days is removed.
- On Linux, the proxy's `dbt parse` runs under `unshare -rnm`, cut off the
  network with the project read-only, when the system allows it; else
  dbt's flags and the guard on packages keep it offline and the project
  as it is.
