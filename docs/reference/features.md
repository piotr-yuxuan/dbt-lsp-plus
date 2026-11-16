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
  macro, generic test, source table and docs block, installed packages
  included;
- the call hierarchy (`textDocument/prepareCallHierarchy`,
  `callHierarchy/incomingCalls`, `callHierarchy/outgoingCalls`): what a
  model, source or macro is used by, and what it
  uses, macros included, with the place of each call.  The dependencies of
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
  block, a model's `name:` to its file, a generic test's name to its test
  block, by dbt 2.0.9's lookup across packages; and from a `source()` in
  SQL; hover (`textDocument/hover`) there shows the docs text, the test's
  signature, the node's summary;
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
server announces stays the server's, but for references and renames on a
source; the initialize result names those under `experimental.dbtLspPlus`.
When `dbt lsp` crashes, the proxy starts it again, at most three times in
ten minutes, and gives it the open documents as they stand.  Without
`dbt lsp`, and while it restarts, the proxy keeps serving its own
features.

With a dbt that has no `dbt lsp`, such as dbt-oss, the proxy answers alone:
see [Use it with dbt-oss alone](../how-to/use-with-dbt-oss.md).
[The design](../explanation/design.md) says how each answer is made.
