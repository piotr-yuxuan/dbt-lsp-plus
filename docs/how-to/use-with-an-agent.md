# Use it with an agent

An agent reaches dbt-lsp-plus as an editor does: its harness starts it as
a language server on the standard input and output, for the `.sql`,
`.yml` and `.yaml` files of a dbt project.  What to give any harness:

- the command: `dbt-lsp-plus`, with the arguments `--`, `dbt`, `lsp`, and
  any flag of [the configuration](../reference/features.md#configuration);
- the files: `.sql` as `sql`, `.yml` and `.yaml` as `yaml`;
- the root: the directory of `dbt_project.yml`, else `--project-dir`.

What it gives an agent: every resource of the project by name
(`workspace/symbol`), what a model, source or macro uses and is used by
(the call hierarchy), references and definitions, from YAML too, the
outline of a file, a hover summary, and the last run's failures as
diagnostics.  The command `dbt-lsp-plus.nodeInfo` gives what dbt knows of
a node as JSON.

## Claude Code

A plugin declares the server in `.lsp.json` at its root:

```json
{
  "dbt": {
    "command": "dbt-lsp-plus",
    "args": ["--", "dbt", "lsp"],
    "extensionToLanguage": {".sql": "sql", ".yml": "yaml", ".yaml": "yaml"}
  }
}
```

## Codex

Codex takes MCP servers, not language servers: an LSP-to-MCP bridge runs
dbt-lsp-plus behind it, declared in `~/.codex/config.toml`:

```toml
[mcp_servers.dbt]
command = "<the bridge>"
args = ["<the bridge's flags>", "dbt-lsp-plus", "--", "dbt", "lsp"]
```

## Other harnesses

A harness with a setting for language servers takes the command, files
and root above.  One that takes MCP servers only takes a bridge, as Codex
does.
