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
  YAML too, each version of a model as `orders.v2`;
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
  included; and from a `source()` in SQL; hover
  (`textDocument/hover`) there shows the docs text, the test's signature,
  the node's summary;
- the last run's failures as diagnostics
  (`textDocument/publishDiagnostics`), beside dbt lsp's own: after a
  `dbt build` from anywhere, a model's error at its file, a test's
  failure at its name in YAML; after `dbt source freshness`, each stale
  source table at its name;
- inlay hints (`textDocument/inlayHint`) after the server's: the relation
  dbt builds, after each `ref` and `source`;
- `dbt-lsp-plus.nodeInfo` (`workspace/executeCommand`), a command that
  gives a node's resolved
  configuration, relation and summary from the user's own `dbt parse`, and
  that summary first in the server's hover on a `ref` or a `source`;
- within a document: folding (`textDocument/foldingRange`) of Jinja
  blocks, CTEs, tags and comments; highlights
  (`textDocument/documentHighlight`) of a CTE's uses and of a block's tags;
  selection ranges (`textDocument/selectionRange`) from a word up to the
  file; the rename (`textDocument/rename`) of a CTE in its file, refused
  when a string in a Jinja tag names it, since a macro may render it as
  SQL.

Every other message passes unchanged, byte for byte.  A feature that the
server announces stays the server's, but for a source's definition,
references and rename, a CTE's rename, and the definitions of refs,
docs, entries (a function's included) and tests in YAML; the initialize
result names those, per kind of target and kind of file, under
`experimental.dbtLspPlus`.
When `dbt lsp` crashes, the proxy starts it again, at most three times in
ten minutes, and gives it the open documents as they stand.  Without
`dbt lsp`, and while it restarts, the proxy keeps serving its own
features.

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
  name, such as `textDocument/hover`: `ours`, `dbt` or `merge` (see
  "Modes").  Repeatable.
- `--default-mode MODE`: the mode of every method the proxy answers but
  those `--mode` sets.
- `--backend auto|never`: `never` starts no `dbt lsp`, and the proxy
  answers alone, as with dbt-oss.  Default: `auto`.
- `--`: ends the proxy's own arguments; needed only before a server
  command that starts with a dash.
- `-h`, `--help`: prints the usage and exits.
- `-V`, `--version`: prints `dbt-lsp-plus X.Y.Z` and exits.

A value goes in the next word or after `=`, as `--backend=never`.

### Modes

Each method the proxy answers has a mode: `ours`, the proxy alone; `dbt`,
`dbt lsp` alone, its answer passed as it is; `merge`, both.  Set nothing
and every method behaves as it always has.  The methods are
`textDocument/documentSymbol`, `workspace/symbol`,
`textDocument/prepareCallHierarchy` (with the call hierarchy's two
others), `textDocument/foldingRange`, `textDocument/documentHighlight`,
`textDocument/selectionRange`, `textDocument/hover`,
`textDocument/inlayHint`, `textDocument/definition`,
`textDocument/references`, `textDocument/rename` and
`textDocument/publishDiagnostics`; any other is `dbt lsp`'s.  Besides the
command line, a client sets them in `initializationOptions`, which win
method by method:

```json
{"dbtLspPlus": {"modes": {"textDocument/hover": "dbt"}, "defaultMode": "merge"}}
```

For the methods the proxy adds (the outline, workspace symbols, the call
hierarchy, folding, highlights and selection ranges), `merge` is the
proxy's answer, as `ours` is, until a later version merges them; `dbt`
leaves the capability as `dbt lsp` announced it, so that a method it
announces as absent is never asked.

In Neovim that is `init_options`, not `settings`: the proxy reads no
`workspace/didChangeConfiguration`, and a change applies when the client
starts again.  A refused key is ignored and shown once in a warning.
`dbt lsp` gets `initializationOptions` without `dbtLspPlus`.  With no
`dbt lsp`, a method set to `dbt` gets nothing, and a warning says so.

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
- A server command without the word `lsp` is run as it is, as a wrapper
  of the user's own: the proxy then runs no `dbt parse`.

### The flags its `dbt parse` keeps

The proxy runs its own `dbt parse` for what it answers from the manifest.
It builds that command from the server command: the same launcher, then
these flags when the server command has them, as `--flag X` or
`--flag=X`.  `dbt lsp` reads them too.

- `--project-dir DIR`: the dbt project.  Default: the client's first
  workspace folder or `rootUri`, when it holds a `dbt_project.yml`; else
  the proxy's working directory.  A relative `DIR` is taken from the
  proxy's working directory.
- `--profiles-dir DIR`: where `profiles.yml` is.  Default, as dbt's: the
  proxy's working directory, then `~/.dbt`, not the project.  An editor
  that starts the proxy in a subdirectory of the project needs it.
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
  wrote for a day is removed.

The server command, `dbt lsp` and the proxy's `dbt parse` inherit the
whole environment.

### Fixed limits

These are not configurable.

- A `dbt parse` is stopped after 120 s.
- The probe of `lsp --help` is stopped after 10 s.
- `workspace/symbol` answers at most 20 000 symbols.
- A message larger than 64 MiB stops the session.
- On Linux, the proxy's `dbt parse` runs under `unshare -rnm`, cut off the
  network with the project read-only, when the system allows it; else
  dbt's flags and the guard on packages keep it offline and the project
  as it is.
