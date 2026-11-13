# dbt-lsp-plus

A language server that sits in front of `dbt lsp`, the language server of
dbt 2, and adds what it lacks, for every editor and for agents:

- `textDocument/documentSymbol`: the CTEs of a model and its Jinja blocks
  (macro, test, snapshot, materialization, docs, set), nested by extent, as
  the buffer stands, unsaved edits included; in a YAML properties file, its
  models, seeds, snapshots, analyses, macros, functions, data tests,
  sources and their tables, columns, exposures, metrics, semantic models,
  saved queries, unit tests and groups;
- `workspace/symbol`: every model, seed, snapshot, analysis, singular test,
  macro, generic test, source table and docs block, installed packages
  included;
- the call hierarchy: what a model, source or macro is used by, and what it
  uses, macros included, with the place of each call.  The dependencies of
  the nodes are those of `dbt.listNodes` once the server has compiled, so
  that `ref(var('x'))` resolves; the edges of macros come from the files;
- references and renames of sources, which dbt lsp answers with nothing:
  every `source()` call, in SQL, YAML, `dbt_project.yml` and Python models,
  and the declarations.  A rename keeps the warehouse relation, inserting
  `identifier:` or `schema:` with the old name, and is refused rather than
  applied in part;
- go to definition from a properties file, where dbt lsp answers nothing: a
  `ref` to its model, a `source` to its table's entry, a `doc` to its docs
  block, a model's `name:` to its file, a generic test's name to its test
  block, by dbt 2.0.9's lookup across packages; and from a `source()` in
  SQL; hover there shows the docs text, the test's signature, the node's
  summary;
- the last run's failures as diagnostics, beside dbt lsp's own: after a
  `dbt build` from anywhere, a model's error at its file, a test's
  failure at its name in YAML;
- inlay hints after the server's: the relation dbt builds, after each
  `ref` and `source`;
- `dbt-lsp-plus.nodeInfo`, a command that gives a node's resolved
  configuration and relation from the user's own `dbt parse`, and a summary
  of the node first in the server's hover on a `ref` or a `source`;
- within a document: folding of Jinja blocks, CTEs, tags and comments;
  highlights of a CTE's uses and of a block's tags; selection ranges from a
  word up to the file; the rename of a CTE in its file, refused when a
  string in a Jinja tag names it, since a macro may render it as SQL.

Every other message passes unchanged, byte for byte.  A feature that the
server announces stays the server's, but for references and renames on a
source; the initialize result names those under `experimental.dbtLspPlus`.
Without `dbt lsp`, or after it crashed, the proxy keeps serving its own
features.

It uses only the server's interface, unlocks nothing that dbt gates behind
an account, and does not ship dbt.  `docs/design.md` explains how it works
and what it would take to finish.

## Usage

```
dbt-lsp-plus [--] [SERVER COMMAND...]
```

The server command defaults to `dbt lsp`.  The project is the
`--project-dir` of the server command, else the client's root folder, else
the current directory.  For example, with lsp-mode or any client that starts
a stdio server:

```
dbt-lsp-plus -- dbt lsp --project-dir /path/to/project
```

## Build and test

```
nix develop
cargo build --release
cargo test
DBT_LSP_PLUS_TEST_DBT=/path/to/dbt cargo test --test proxy -- --ignored
```

The last command runs the opt-in live test against a real dbt 2 (tested
with 2.0.9), on a copy of `tests/fixtures/shop` with a scratch home.

The crate also builds `fake-dbt-lsp`, the scripted server of the tests; to
install the proxy alone, run `cargo install --path . --bin dbt-lsp-plus`.

## Licence

European Union Public Licence 1.2 (`LICENSE`) or GNU General Public
License 3.0 or later (`COPYING`).
