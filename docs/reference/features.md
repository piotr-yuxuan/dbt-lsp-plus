# Features

What dbt-lsp-plus answers, from the project's files and the user's own
`dbt parse`, by LSP method:

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
  uses, macros and functions included, with the place of each call.  The
  dependencies of the nodes are those of the last parse's manifest once
  there is one, so that `ref(var('x'))` resolves; the edges of macros
  come from the files;
- references and renames of sources (`textDocument/references`,
  `textDocument/rename`): every `source()` call, in SQL, YAML,
  `dbt_project.yml` and Python models, and the declarations.  A rename
  keeps the warehouse relation, inserting `identifier:` or `schema:` with
  the old name, and is refused rather than applied in part;
- go to definition (`textDocument/definition`) from a properties file: a
  `ref` to its model, a `source` to its table's entry, a `doc` to its docs
  block, a model's or a function's `name:` to its file, a generic test's
  name to its test block, by dbt 2.0.9's lookup across packages, versions
  included; and from a `source()` in SQL; and, with references
  (`textDocument/references`), from a `ref` in SQL or a Python model, a
  macro call and a `function()`, a package's `ref`, a version and the
  references in other files included; hover (`textDocument/hover`) there
  shows the docs text, the test's signature, the node's summary;
- diagnostics (`textDocument/publishDiagnostics`): the last run's
  failures, after a `dbt build` from anywhere, a model's error at its
  file, a test's failure at its name in YAML, and after `dbt source
  freshness`, each stale source table at its name; and the errors and
  warnings of the last `dbt parse` (source `dbt parse`);
- inlay hints (`textDocument/inlayHint`): the relation dbt builds, after
  each `ref` and `source`;
- `dbt-lsp-plus.compile` and `dbt-lsp-plus.nodes`
  (`workspace/executeCommand`), commands for clients in the shapes of dbt
  lsp 2.0.9's `dbt.compileFile` and `dbt.listNodes`, by dbt's CLI, as the
  user runs dbt.  A compile is refused where a package may
  have `on-run-start` or `on-run-end` hooks, which `dbt compile` runs
  against the warehouse; a compile of a model that introspects the
  warehouse, by `run_query` under `execute` or a macro such as
  `dbt_utils.star`, or of a Python model, queries it, and dbt may
  download its driver;
- `dbt-lsp-plus.nodeInfo` (`workspace/executeCommand`), a command that
  gives a node's resolved
  configuration, relation and summary from the user's own `dbt parse`,
  with `children`, the unique ids of the enabled nodes that depend on it,
  every test aside, and
  that summary as the hover on a `ref` or a `source`, and
  at line 1, column 1 (position 0:0) of a file that defines one node, a
  YAML file aside (a properties file, or a YAML snapshot's),
  followed by the node's facts, a line each, each value a code span: its
  unique id and properties file, then its declared columns with their
  types.  A line break inside a value is given as a space.  A package's
  file is written `<package>://<path>`, as dbt writes `patch_path`: the
  path is relative to the package's directory, under `dbt_packages/` by
  default.  A hover on a node before the first parse has given an index
  is error -32803 with nodeInfo's reason, `dbt parse has not finished:
  ...`;
- within a document: folding (`textDocument/foldingRange`) of Jinja
  blocks, CTEs, tags and comments; highlights
  (`textDocument/documentHighlight`) of a CTE's uses and of a block's tags;
  selection ranges (`textDocument/selectionRange`) from a word up to the
  file; the rename (`textDocument/rename`) of a CTE in its file, refused
  when a string in a Jinja tag names it, since a macro may render it as
  SQL.

Definitions and references are answered at every position, in any file,
as dbt's lookup resolves the name there; a position that names no
target, or a document that does not read, gets their nothing, null or
`[]`, and a rename at no source and no CTE gets null.  References at a
`doc()` or at a generic test's name in YAML find no use: `[]`, or the
declaration alone with `includeDeclaration`.
A method or a command that dbt-lsp-plus does not answer, such as
`textDocument/completion`, gets error -32601 `dbt-lsp-plus does not
answer <name>`.  When the root's `dbt_project.yml` no longer reads, a
definition or references on a target, workspace symbols, the call
hierarchy and, once a parse has run, a hover and a nodeInfo at a `ref`,
a `source` or an entry's `name:` fail with `no dbt project at ...`.
With a dbt that cannot run, its parse fails, and `nodeInfo` and a hover
on a node give its failure.

Once a session has a project, a hover, a definition, references, a call
hierarchy or a nodeInfo at a position on a file of another dbt project
fails with
`<file> lies in the dbt project at <other>, and this session serves only
the one at <root>`.

[The design](../explanation/design.md) says how each answer is made.

## Configuration

dbt-lsp-plus reads no configuration file, nor the client's
`initializationOptions`: what it takes is on its command line, which
ends with the server command, and in its environment.  Every option at
once, for a project whose profile sits beside it:

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

### The arguments

```
dbt-lsp-plus [--] [SERVER COMMAND...]
dbt-lsp-plus -h|-V
```

A first word that starts with `-`, but `--`, is an option; any other
option is a usage error: the reason and the usage on standard error,
exit status 2, nothing started.

- `--`: ends dbt-lsp-plus's own arguments; needed only before a server
  command that starts with a dash, or whose first word is `doctor`,
  `select` or `node`, which [name a subcommand](cli.md).
- `-h`, `--help`: prints the usage and exits.
- `-V`, `--version`: prints `dbt-lsp-plus X.Y.Z` and exits.

### Methods and commands

Each method and command that dbt-lsp-plus answers; any other gets error
-32601.  Its nothing is its answer where it finds nothing.  One that asks
for a parse starts one first when the index is stale, so that its
reason, if it cannot answer, is that parse's; inlay hints ask for one
after answering, while there is no index.  One that reads the project
fails on a document of another dbt project with the reason above,
nodeInfo when its argument is a position there; inlay hints get `[]`
there.

| Method or command | Its nothing | Asks for a parse | Reads the project |
|---|---|---|---|
| `textDocument/documentSymbol` | null | no | no |
| `workspace/symbol` | `[]` | no | no |
| `textDocument/prepareCallHierarchy` | null | yes | yes |
| `callHierarchy/incomingCalls` | null | yes | no |
| `callHierarchy/outgoingCalls` | null | yes | no |
| `textDocument/foldingRange` | null | no | no |
| `textDocument/documentHighlight` | null | no | no |
| `textDocument/selectionRange` | null | no | no |
| `textDocument/hover` | null | yes | yes |
| `textDocument/inlayHint` | `[]` | no | yes |
| `textDocument/definition` | null | yes | yes |
| `textDocument/references` | `[]` | yes | yes |
| `textDocument/rename` | null | no | no |
| `dbt-lsp-plus.nodeInfo` | null | yes | yes |
| `dbt-lsp-plus.compile` | null | no | no |
| `dbt-lsp-plus.nodes` | null | no | no |

### The server command

- `SERVER COMMAND...`: the user's dbt and the flags of its parse, as
  `dbt lsp` would take them.  Default: `dbt lsp`.  It never runs as such:
  dbt-lsp-plus speaks LSP itself on its standard input and output.
- The words before `lsp` are how dbt is launched: `dbt`, a path to a dbt
  executable, `uvx dbt`, or `env X=1 dbt`.
- Without `--project-dir`, when the first word is the bare `dbt` (the
  default included), dbt-lsp-plus runs the project's dbt: the first of the
  project's `.venv/bin/dbt`, the repository's, and, from a linked
  worktree, the main worktree's project's and the main worktree's own,
  that answers `lsp --help`, run for at most 10 s, as dbt 2 does, with its
  help or dbt-oss's refusal of `lsp`; one that answers otherwise, as
  dbt-core 1.x does, or cannot run, is skipped.  Else the `dbt` on
  `PATH`.  Any other first word is kept as written.  It says once, on its
  standard error and in a `window/logMessage`, `dbt-lsp-plus: project
  <root>, dbt <path> (<where it was found>).`, with each place skipped.
  With `--project-dir`, the command runs as written, and the line is
  `dbt-lsp-plus: project <root>, dbt <words before lsp> (the server
  command).`.
- A server command without the word `lsp` is kept as it is, as a wrapper
  of the user's own: dbt-lsp-plus then runs no dbt.

### The flags its `dbt parse` keeps

dbt-lsp-plus runs its own `dbt parse` for what it answers from the
manifest.  It builds that command from the server command: the same
launcher, then these flags when the server command has them, as `--flag
X` or `--flag=X`.

- `--project-dir DIR`: the dbt project.  A relative `DIR` is taken from
  the server's working directory.  Default: the client's first workspace
  folder or `rootUri`, when it holds a `dbt_project.yml`; else the
  project of the first file opened in one: the nearest directory above it
  holding a `dbt_project.yml` that lies in no package, installed or local,
  of a project above it.  Before that the server runs nothing, answers
  each request of its own with null, and any other with -32601.  The
  working directory never names the project.
- `--profiles-dir DIR`: where `profiles.yml` is.  Default, as dbt's:
  dbt's working directory, then `~/.dbt`.  dbt runs in the project when
  the client's folder or a file named it, so a `profiles.yml` beside
  `dbt_project.yml` is found; with `--project-dir`, in the server's
  working directory, not the project's: an editor that starts the server
  in a subdirectory of the project needs it.  A relative `DIR`, as a
  relative path to dbt in the server command, is taken from the server's
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

Every other flag is not read.  The parse also gets
`--target-path` and `--log-path` in the server's cache, `--write-json`,
`--log-format json`, `--quiet`, `--no-version-check`,
`--no-send-anonymous-usage-stats` and `--no-manage-state`; none of these
can be changed.

### The environment

- `DBT_PACKAGES_INSTALL_PATH`: where the packages are installed, below
  `--packages-install-path` and above `dbt_project.yml`.  Read from the
  server's environment, or from a word `DBT_PACKAGES_INSTALL_PATH=DIR`
  before `lsp` in the server command, which wins.  An empty value counts
  as unset.
- `DBT_TARGET_PATH`: where the server watches the user's last run
  (`run_results.json`, `sources.json` and `manifest.json`), relative to
  the project.  Default: `target`.
- `XDG_CACHE_HOME`: where the server keeps its parses, under
  `dbt-lsp-plus/`.  Default: `$HOME/.cache`, else the system's temporary
  directory.  A relative value counts as unset.  A cache that no parse
  wrote for a day is removed.

The server's runs of dbt, its `dbt parse` and the commands', inherit the
whole environment.

### Fixed limits

These are not configurable.

- A `dbt parse` is stopped after 120 s.
- The probe of `lsp --help` is stopped after 10 s.
- `workspace/symbol` answers at most 20 000 symbols.
- A frame that the server cannot read as one LSP message, a JSON-RPC batch
  included, ends the session.
- The server's runs of dbt, its `dbt parse` and the commands', run as the
  user runs dbt, with dbt's flags against its statistics and its version
  check.  The guard on packages refuses such a run where a local or a
  locked package is not installed, a package from the network is not in
  a lock as recent as the files naming it, or a file naming packages, or
  `dbt_project.yml`, does not read.  A lock as recent but stale in
  content passes, and dbt then installs anew, rewriting the install path
  ([how the server runs
  dbt](../explanation/design.md#how-the-server-runs-it)).
