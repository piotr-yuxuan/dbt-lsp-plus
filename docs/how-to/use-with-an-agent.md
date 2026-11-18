# Use it with an agent

An agent reaches dbt-lsp-plus as an editor does: its harness starts it as
a language server on the standard input and output, for the `.sql`,
`.yml` and `.yaml` files of a dbt project.  What to give any harness:

- the command: `dbt-lsp-plus`, and nothing more: `dbt lsp` is the
  default, and its `dbt` is found from the project;
- the files: `.sql` as `sql`, `.yml` and `.yaml` as `yaml`;
- no path: the project is the harness's folder when it holds
  `dbt_project.yml`, else the one of the first file the agent opens in a
  project, as in a repository whose project is in `dbt/` or in a
  worktree.  Outside a dbt project the proxy answers nothing, and runs
  nothing.

With the bare `dbt` (the default), the proxy runs the first of these that
answers `dbt lsp --help` as dbt 2 does: the project's `.venv/bin/dbt`,
the repository's, the main worktree's project's and the main worktree's
own (from a linked worktree), else the `dbt` on `PATH`.  A dbt-core 1.x
found there is skipped.  It says once, on its standard error and in its
log, which project and which dbt it took.

What it gives an agent: every resource of the project by name
(`workspace/symbol`), what a model, source or macro uses and is used by
(the call hierarchy), references and definitions, from YAML too, the
outline of a file, a hover summary, and the last run's failures as
diagnostics.  The command `dbt-lsp-plus.nodeInfo` gives what dbt knows of
a node as JSON.  On a project found from a file, rather than from the
harness's folder, the proxy answers alone, as with dbt-oss: from the
files and its own `dbt parse`, without `dbt lsp`.

## Claude Code

The repository is a plugin marketplace whose one plugin, `dbt-lsp-plus`,
declares the server.  With `dbt-lsp-plus` installed and on the `PATH` that
Claude Code starts with:

```
claude plugin install dbt-lsp-plus --marketplace piotr-yuxuan/dbt-lsp-plus
```

which adds the marketplace first, as `claude plugin marketplace add
piotr-yuxuan/dbt-lsp-plus` then `claude plugin install
dbt-lsp-plus@dbt-lsp-plus` do; a clone's path works in place of
`piotr-yuxuan/dbt-lsp-plus`.  In a session that was running, then,
`/reload-plugins`.  Installed at user scope, the default, it serves the
`.sql`, `.yml` and `.yaml` files of every repository, and stays quiet
where there is no dbt project; with `--scope project`, only in the
repository where it is installed.  Its `.lsp.json` holds no path:

```json
{
  "dbt": {
    "command": "dbt-lsp-plus",
    "extensionToLanguage": {".sql": "sql", ".yml": "yaml", ".yaml": "yaml"}
  }
}
```

Claude Code starts the server at the first LSP call, or the first `Edit`,
on such a file, in the session's directory, with its own environment.
An agent loads the LSP tool with ToolSearch first.  A hover asked within a
second of the first file opened may come back empty while the proxy's
first `dbt parse` runs.

When the LSP tool answers `Executable not found in $PATH:
"dbt-lsp-plus"`, Claude Code did not find the binary: install it where
the `PATH` of the shell or the desktop app that starts Claude Code
reaches, then start the session again.  Claude Code keeps the server's
standard error only in its debug log: `claude --debug-file /tmp/debug.log`
shows `Failed to start LSP server` when the binary is missing, and the
proxy's line `[LSP SERVER plugin:dbt-lsp-plus:dbt] dbt-lsp-plus: project
..., dbt ... (the project's .venv)` once a file in a project was opened.
A repository with no dbt project gives "No symbols found" and no such
line.

## Codex

Codex takes MCP servers, not language servers: an LSP-to-MCP bridge runs
dbt-lsp-plus behind it, declared in `~/.codex/config.toml`:

```toml
[mcp_servers.dbt]
command = "<the bridge>"
args = ["<the bridge's flags>", "dbt-lsp-plus"]
```

## Other harnesses

A harness with a setting for language servers takes the command and files
above.  One that takes MCP servers only takes a bridge, as Codex does.
