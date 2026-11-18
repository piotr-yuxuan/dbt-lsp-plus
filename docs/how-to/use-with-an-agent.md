# Use it with an agent

An agent reaches dbt-lsp-plus as an editor does: its harness starts it
as a language server for the `.sql`, `.yml` and `.yaml` files of a dbt
project.  The command is `dbt-lsp-plus` and nothing more: the project is
the session's directory when it holds `dbt_project.yml`, else the one of
the first file opened in a project, as a nested `dbt/` or a worktree;
its dbt is the project's `.venv/bin/dbt`, the repository's or the main
worktree's, else the `dbt` on `PATH`.  Outside a dbt project it answers
nothing and runs nothing.

## Install

The binary, with Rust 1.89 or later and a C linker, where the `PATH`
that starts Claude Code reaches it:

```
cargo install --locked --git https://github.com/piotr-yuxuan/dbt-lsp-plus --bin dbt-lsp-plus
dbt-lsp-plus --version
```

Then the Claude Code plugin, from the repository, a marketplace of one
plugin:

```
claude plugin marketplace add piotr-yuxuan/dbt-lsp-plus
claude plugin install dbt-lsp-plus@dbt-lsp-plus
```

For a branch or a local checkout, give the path of a clone to
`marketplace add` instead.  In a session that was running, then,
`/reload-plugins`: the field sessions needed no restart.  Installed at
user scope, the default, the plugin serves every repository and stays
quiet where there is no dbt project.  It holds only a `.lsp.json` that
names the command, with no version and no path: to update, run the
`cargo install` line again.

The plugin runs the default `dbt lsp`, with which dbt 2.0.9 sends its
anonymous usage statistics.  To stop them, install a plugin of your own
in place of this one, since an extension goes to one server.  A
directory `my-plugins` holds `.claude-plugin/marketplace.json`:

```json
{
  "name": "my-plugins",
  "owner": {"name": "me"},
  "plugins": [{"name": "dbt", "source": "./dbt"}]
}
```

`dbt/.claude-plugin/plugin.json` holds `{"name": "dbt"}`, and
`dbt/.lsp.json`:

```json
{
  "dbt": {
    "command": "dbt-lsp-plus",
    "args": ["--", "dbt", "lsp", "--no-send-anonymous-usage-stats"],
    "extensionToLanguage": {".sql": "sql", ".yml": "yaml", ".yaml": "yaml"}
  }
}
```

```
claude plugin uninstall dbt-lsp-plus@dbt-lsp-plus
claude plugin marketplace add ./my-plugins
claude plugin install dbt@my-plugins
```

The proxy runs the dbt it finds in a repository's `.venv`s as soon as a
file of a dbt project opens.  In a repository you do not trust, a
server command that names a dbt by its absolute path is run as written,
in place of the lookup; that form was not tried in Claude Code.

## Check it works in one call

Load the LSP tool with ToolSearch, then ask for `hover` at line 1,
character 1 of a model's file, such as `models/marts/orders.sql`.  The
server starts at that call.  The answer says:

- **It works**: the model's summary, then its facts, the first a line
  that begins with its unique id, as `model table,
  "shop"."main"."orders"`, `1 test`, `` `model.shop.orders` ``.
- **It works, the first parse is under way**: `Error performing hover:
  LSP request 'textDocument/hover' failed for server
  'plugin:dbt-lsp-plus:dbt': dbt parse has not finished: it is
  running`.  Ask again: three seconds later, the fixture's project gave
  the summary.
- **The project and its dbt are found, the parse fails or is
  refused**: `dbt parse has not finished:` then other words, such as
  `dbt parse failed: ...` with dbt's first error and the path of its
  log.  Symbols and navigation still answer; see [When nothing
  answers](#when-nothing-answers).
- **Nothing to show yet, or here**: `No hover information available`.
  In front of `dbt lsp` before its first compilation; in a YAML file or
  a file that defines no single node; or with no project found.  Ask
  `workspaceSymbol` next, then run `dbt-lsp-plus doctor` on the file.
- **No binary**: `Executable not found in $PATH: "dbt-lsp-plus"`.
- **No LSP plugin at all** in the session: ToolSearch answers `No
  matching deferred tools found` and the call `No such tool available:
  LSP. LSP is disabled for this session`.  With other LSP plugins and
  not this one, a `.sql` file gives another error, not recorded here:
  `claude plugin list` names the plugins.

## Who answers what

A session is **alone** with a dbt that has no `lsp` (dbt-oss), on a
project found from a file (a session whose directory holds no
`dbt_project.yml`, as at a repository's or a worktree's root over a
nested `dbt/`), or with `--backend never`; it is **in front of dbt lsp**
with a dbt that has `lsp`, the session opened in the project's folder,
a worktree's whose root holds `dbt_project.yml` included, or given
`--project-dir`.  Claude Code's debug log tells them apart: alone, the
proxy's line ends `dbt-lsp-plus answers alone.`.  So does
`dbt-lsp-plus doctor` with no PATH, run in the session's own directory,
before any `cd`: there, `"alone":null` is in front of dbt lsp, and the
error `no dbt_project.yml at or above ...` says that the project comes
from a file, so the session is alone.  doctor on a file always takes the
file's route, and shows `--backend never` only when given the server
command after `--`.

| Operation | LSP method | Alone | In front of dbt lsp |
|-----------|------------|-------|---------------------|
| goToDefinition | `textDocument/definition` | dbt-lsp-plus at the kinds below, else null | dbt-lsp-plus at the kinds below; else dbt lsp's answer within 2 s, else null |
| findReferences | `textDocument/references` | dbt-lsp-plus at the kinds below, else `[]` | dbt-lsp-plus at the kinds below; else dbt lsp's answer within 2 s, else `[]` |
| hover | `textDocument/hover` | dbt-lsp-plus: a node's summary and facts, a docs block, a test's signature; before the first parse, the error above | dbt lsp's answer within 300 ms, under dbt-lsp-plus's summary where it has one, else within 2 s; past that, dbt-lsp-plus's summary, else null |
| documentSymbol | `textDocument/documentSymbol` | dbt-lsp-plus | dbt-lsp-plus, since dbt lsp 2.0.9 does not announce it; not observed |
| workspaceSymbol | `workspace/symbol` | dbt-lsp-plus | dbt-lsp-plus |
| goToImplementation | `textDocument/implementation` | none: `dbt lsp is not running` | dbt lsp; 2.0.9 answers `No such method textDocument/implementation` |
| prepareCallHierarchy, incomingCalls, outgoingCalls | `textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`, `callHierarchy/outgoingCalls` | dbt-lsp-plus | dbt-lsp-plus for prepareCallHierarchy; incomingCalls and outgoingCalls not observed |

dbt-lsp-plus's answers carry a node's unique id: a symbol reads
`orders (Class) - Line 1 in model.shop.orders`, a call hierarchy item
ends `[model.shop.orders]`.  The mode `plus` changes little for an
agent: the default, `merge`, already gives dbt-lsp-plus's answer wherever
it has one ([the modes](../reference/features.md#modes)).

### Where dbt-lsp-plus answers definitions and references

In the modes `merge`, the default, and `plus`, at these kinds of
target, by the file the position is in, dbt-lsp-plus answers itself,
with dbt lsp or without; the answer to `initialize` lists them under
`capabilities.experimental.dbtLspPlus`.  A request set to `dbt` lists
none.

| request | kind | places | meaning |
|---------|------|--------|---------|
| definition | `source` | sql, yaml, python | a `source()` argument, a source's or table's `name:` |
| definition | `ref` | sql, yaml, python | a `ref()` argument, a unit test's `model:` |
| definition | `macro` | sql, yaml | a macro call, the name in a macro's or a generic test's tag |
| definition | `doc` | yaml | a `doc()` argument |
| definition | `entry` | yaml | an entry's `name:`, a function's included |
| definition | `test` | yaml | a generic test's name |
| definition | `function` | sql, yaml | a `function()` argument |
| references | `source` | sql, yaml, python | as for definition |
| references | `ref` | sql, yaml, python | as for definition |
| references | `macro` | sql, yaml | as for definition |
| references | `function` | sql, yaml | as for definition |

After an `Edit` of a file the server has open, the parse's problems
reach the agent's context unasked, after a later tool result, as
`<new-diagnostics>The following new diagnostic issues were detected:`
then each problem, such as `[DependencyNotFound (dbt1048)]: Ref
'stg_missing' not found in project`.

The LSP tool cannot reach nodeInfo, rename, inlay hints, completion or
compiled SQL.  The hover at line 1, character 1 gives a node's facts;
`dbt-lsp-plus select` lists nodes with their configuration, where dbt
can be cut off the network (below).

## From a shell

`dbt-lsp-plus doctor PATH` says, in one line of JSON, what the server
would do for the file PATH: `route`, `project`, `dbt` (`command`,
`named`, `source`, `skipped`, `lsp`, `why`), `alone`, `cut` and
`packages`; with no PATH, for the current directory.  `dbt-lsp-plus
select SELECTOR`, run in the project, gives
`{"error":null,"nodes":[...]}`, each node as `dbt list` prints it.
select runs dbt only cut off the network, which needs Linux with user
namespaces: elsewhere, as on macOS, it answers `dbt-lsp-plus runs dbt
only cut off the network, which this system cannot`, and doctor's `cut`
is `false`.  doctor exits 0 when it finds a project and a dbt 2, select
when `error` is null; 1 otherwise; 2 for a usage error ([the
subcommands](../reference/cli.md)).

From a repository's root over `dbt/`, doctor on a model's file gave
`"route":"document"`, `"source":"the project's .venv"` and
`"alone":"dbt lsp is not started for a project found from a file"`, and
with no PATH, `no dbt_project.yml at or above <root>; give a path within
the project`.  Run select in the project:

```
cd dbt && dbt-lsp-plus select orders
```

Claude Code's Bash keeps the directory a `cd` enters, so later calls run
in `dbt` too.  From the root, `dbt-lsp-plus select orders -- dbt lsp
--project-dir dbt` runs the `dbt` on `PATH`, not the `.venv`'s, in the
current directory: it failed with `no profiles.yml found` where
`profiles.yml` is in `dbt/`.

Allow both, in `--allowedTools` or the settings' permissions, as
`Bash(dbt-lsp-plus doctor:*)` and `Bash(dbt-lsp-plus select:*)`; without
them, Claude Code answers `This command requires approval`.

## When nothing answers

By what the agent reads:

- `Executable not found in $PATH: "dbt-lsp-plus"`: install the binary
  where the `PATH` of the shell or the desktop app that starts Claude
  Code reaches, then start the session again.
- `No such tool available: LSP`: no LSP plugin is enabled in the
  session.  Install this one, then `/reload-plugins`.
- `No symbols found in workspace` from `workspaceSymbol`: no project was
  found from that file, or no resource's name holds the query's letters
  in order.  Ask again with one letter of a model's name, then run
  `dbt-lsp-plus doctor` on the file.
- `dbt parse has not finished: it is running`: ask again.
- `dbt parse has not finished: waiting for dbt lsp to compile the
  project`: in `plus`, in front of `dbt lsp`; ask again.
- `dbt parse has not finished: dbt parse failed: ...`: dbt's first error;
  its log is the path in parentheses.  `Read-only file system (os error
  30)` there, from a project that has a `.gitignore` without the line
  `dbt_internal_packages/`: dbt 2 adds that line, and the proxy runs its
  parse with the project read-only.  Add the line; a project with no
  `.gitignore` is unaffected.
- `dbt parse has not finished:` then `... run dbt deps`: a package is
  missing or its lock is stale, and the proxy will not let its parse
  install it.  Run `dbt deps`.
- `dbt parse has not finished: a package from the network is named, and
  nothing cuts dbt parse off the network here`: the system cannot cut
  dbt off the network, as select cannot.
- `dbt-lsp-plus runs dbt only cut off the network, which this system
  cannot`, from select: see [From a shell](#from-a-shell).
- `No hover information available` in front of `dbt lsp`: its first
  compilation is under way; ask again.
- goToImplementation's `dbt lsp is not running`, alone: only `dbt lsp`
  answers it; nothing to fix.
- Answers older than an `Edit`: an `Edit` of a file the server has not
  opened, other than the first, opens it without saving it, and starts
  no parse.  Make an LSP call on a file before editing it; once the
  answers are stale, edit it again, which saves an open file and starts
  a parse.

Three setups fail silently.  A `.lsp.json` at a repository's root does
nothing: Claude Code reads it at a plugin's root only.  A plugin
installed at user scope names no `--project-dir`, since each session
finds its project.  A session finds one project, once, from its
directory or from the first file opened in a project: for a worktree,
start the session in the worktree.

Claude Code keeps the server's standard error only in its debug log:
`claude --debug-file /tmp/debug.log` shows `Failed to start LSP server`
when the binary is missing, and, once a file of a project opened, the
line `[LSP SERVER plugin:dbt-lsp-plus:dbt] dbt-lsp-plus: project ...,
dbt ... (the project's .venv)`.

## Codex and other harnesses

Codex takes MCP servers, not language servers: an LSP-to-MCP bridge runs
dbt-lsp-plus behind it, declared in `~/.codex/config.toml`:

```toml
[mcp_servers.dbt]
command = "<the bridge>"
args = ["<the bridge's flags>", "dbt-lsp-plus"]
```

A harness with a setting for language servers takes the command and the
files above: `.sql` as `sql`, `.yml` and `.yaml` as `yaml`.  One that
takes MCP servers only takes a bridge, as Codex does.
