# Use it with dbt-oss alone

dbt-oss 2.0.x has no `dbt lsp`.  With it, the proxy is a language server on
its own: everything in [the features](../reference/features.md) that needs
no server, plus, from your own `dbt
parse`, run offline at each save or change of the project, node
information, the hover summary, inlay hints, the call hierarchy as dbt
resolves it, and dbt's errors and warnings as diagnostics (source `dbt
parse`).  It says so once in the log, and warns of nothing.  Go to
definition and references cover, as in front of `dbt lsp`, a `ref` and a
macro call in SQL, a
`ref` in a Python model, a `function()` call in SQL or YAML, and a macro
call in YAML: across the project, versions included.  Formatting,
completion, compiled SQL and previews are `dbt lsp`'s, and absent.

The command is the same, `dbt-lsp-plus -- dbt lsp --project-dir
/path/to/project`: the proxy asks `dbt lsp --help` first.  Point it at the
dbt executable itself rather than a launcher such as `uvx dbt`: on Linux the
proxy runs `dbt parse` with no network and the project read-only, under
`unshare -rnm`, where a launcher fails when its cache is cold.

[The design](../explanation/design.md#standalone-mode) says what the proxy
answers alone, and how it runs `dbt parse`.
