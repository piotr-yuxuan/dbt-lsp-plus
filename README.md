# dbt-lsp-plus

A language server that sits in front of `dbt lsp`, the language server of
dbt 2, and adds what it lacks, for every editor and for agents:

- `textDocument/documentSymbol`: the CTEs of a model and its Jinja blocks
  (macro, test, snapshot, materialization, docs, set), nested by extent, as
  the buffer stands, unsaved edits included;
- `workspace/symbol`: every model, seed, snapshot, analysis, singular test,
  macro, generic test, source table and docs block, installed packages
  included;
- the call hierarchy: what a model, source or macro is used by, and what it
  uses, macros included, with the place of each call.  The edges between
  nodes are those of `dbt.listNodes` once the server has compiled, so that
  `ref(var('x'))` resolves; the edges to macros come from the files.

Every other message passes unchanged, byte for byte.  A feature that the
server announces stays the server's.  Without `dbt lsp`, or after it
crashed, the proxy keeps serving its own features.

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

## Licence

European Union Public Licence 1.2 (`LICENSE`) or GNU General Public
License 3.0 or later (`COPYING`).
