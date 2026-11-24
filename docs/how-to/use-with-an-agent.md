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

The plugin passes no argument: the server command is the default `dbt
lsp`.  To give flags of your own, such as a target, install a plugin of
your own in place of this one, since an extension goes to one server.  A
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
    "args": ["--", "dbt", "lsp", "--target", "dev"],
    "extensionToLanguage": {".sql": "sql", ".yml": "yaml", ".yaml": "yaml"}
  }
}
```

```
claude plugin uninstall dbt-lsp-plus@dbt-lsp-plus
claude plugin marketplace add ./my-plugins
claude plugin install dbt@my-plugins
```

dbt-lsp-plus runs the dbt it finds in a repository's `.venv`s as soon as a
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
  In a YAML file or a file that defines no single node; or with no
  project found.  Ask
  `workspaceSymbol` next, then run `dbt-lsp-plus doctor` on the file.
- **No binary**: `Executable not found in $PATH: "dbt-lsp-plus"`.
- **No LSP plugin at all** in the session: ToolSearch answers `No
  matching deferred tools found` and the call `No such tool available:
  LSP. LSP is disabled for this session`.  With other LSP plugins and
  none that runs dbt-lsp-plus, a `.sql` file gives an answer without a
  unique id, or another error, not recorded here: `claude plugin list`
  names the plugins.  A plugin of your own that runs dbt-lsp-plus names
  its server `plugin:<plugin>:<server>` in the messages above.

## Who answers what

dbt-lsp-plus answers every request itself, from the files and the last
parse's index, whichever dbt 2 the session runs; a method it does not
answer gets error -32601 at once.  `dbt-lsp-plus doctor` says which
project and which dbt a session takes, with no PATH, run in the
session's own directory, before any `cd`, given after `--` the arguments
that the plugin's `.lsp.json` passes; this plugin passes none.  A
non-null `dbt.why` means its dbt does not answer as a dbt 2 does; the
error `no dbt_project.yml at or above ...` means no project.  doctor on a
file takes the file's route unless the arguments after `--` name
`--project-dir`.

| Operation | LSP method | Answer |
|-----------|------------|--------|
| goToDefinition | `textDocument/definition` | dbt-lsp-plus; null where the position names no target |
| findReferences | `textDocument/references` | dbt-lsp-plus; `[]` where the position names no target, and at a `doc()` or a generic test's name (below) |
| hover | `textDocument/hover` | a node's summary and facts, a docs block, a test's signature; before the first parse, the error above |
| documentSymbol | `textDocument/documentSymbol` | dbt-lsp-plus |
| workspaceSymbol | `workspace/symbol` | dbt-lsp-plus |
| goToImplementation | `textDocument/implementation` | none: `dbt-lsp-plus does not answer textDocument/implementation` |
| prepareCallHierarchy, incomingCalls, outgoingCalls | `textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`, `callHierarchy/outgoingCalls` | dbt-lsp-plus |

dbt-lsp-plus's answers carry a node's unique id: a symbol reads
`orders (Class) - Line 1 in model.shop.orders`, a call hierarchy item
ends `[model.shop.orders]`.  A definition answers at any target that
dbt's lookup resolves, in any file: a `source()` or a `ref()` argument,
a unit test's `model:`, a macro call, a `doc()`, an entry's `name:`, a
generic test's name, a `function()` argument.  References answer at the
same targets but a `doc()` and a generic test's name in YAML, where they
find no use: `[]`, or the declaration alone with `includeDeclaration`.

After an `Edit` of a file the server has open, the parse's problems
reach the agent's context unasked, after a later tool result, as
`<new-diagnostics>The following new diagnostic issues were detected:`
then each problem, such as `[DependencyNotFound (dbt1048)]: Ref
'stg_missing' not found in project`.

The LSP tool cannot reach nodeInfo, rename, inlay hints, completion or
compiled SQL.  The hover at line 1, character 1 gives a node's facts;
`dbt-lsp-plus node` gives what nodeInfo gives, and `dbt-lsp-plus select`
lists nodes with their configuration (below).

## From a shell

`dbt-lsp-plus doctor PATH` says, in one line of JSON, what the server
would do for the file PATH: `route`, `project`, `dbt` (`command`,
`named`, `source`, `skipped`, `why`) and `packages`;
with no PATH, for the current directory.  `dbt-lsp-plus select
SELECTOR`, run in the project, gives `{"error":null,"nodes":[...]}`,
each node as `dbt list` prints it.  `dbt-lsp-plus node ID|PATH[:LINE[:COLUMN]]` gives what
nodeInfo gives of the node that a unique id names, in the project of the
current directory, or that a place in a file names, as
`{"error":null,"node":{...}}`, the node with its `summary` and its
`children`, the unique ids of the enabled nodes that depend on it.
`dbt-lsp-plus lineage ID [--depth N]` gives the nodes up and down from
the node ID, within N hops, every hop without `--depth`, as
`{"error":null,"lineage":{"upstream":{...},"downstream":{...}}}`, each a
map of unique id to its fewest hops.  The first run of node or lineage
in a project runs a `dbt parse` and keeps its manifest; the next runs
read it, while the project's files stay as they were.  Like the
server's, that parse adds the line `dbt_internal_packages/` to a
`.gitignore` of the project that lacks it, as dbt 2 does.  doctor
exits 0 when it finds a project and a dbt 2, select, node and lineage
when `error` is null; 1 otherwise; 2 for a usage error ([the
subcommands](../reference/cli.md)).

From a repository's root over `dbt/`, doctor on a model's file gave
`"route":"document"` and `"source":"the project's .venv"`, and with no
PATH, `no dbt_project.yml at or above <root>; give a path within
the project`.  Run select in the project:

```
cd dbt && dbt-lsp-plus select orders
```

Claude Code's Bash keeps the directory a `cd` enters, so later calls run
in `dbt` too.  From the root, `dbt-lsp-plus select orders -- dbt lsp
--project-dir dbt` runs the `dbt` on `PATH`, not the `.venv`'s, in the
current directory: it failed with `no profiles.yml found` where
`profiles.yml` is in `dbt/`.

Allow them, in `--allowedTools` or the settings' permissions, as
`Bash(dbt-lsp-plus doctor:*)`, `Bash(dbt-lsp-plus select:*)`,
`Bash(dbt-lsp-plus node:*)` and `Bash(dbt-lsp-plus lineage:*)`; without
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
- `dbt parse has not finished: it has not run, or left no manifest`: no
  parse has started yet, or the last one ended well and wrote no
  manifest; ask again, then run `dbt-lsp-plus doctor` on the file.
- `dbt parse has not finished: dbt parse failed: ...`: dbt's first error;
  its log is the path in parentheses.
- `dbt parse has not finished:` then `... run dbt deps`: a package is
  missing or its lock is stale, and the server will not let its parse
  install it.  Run `dbt deps`.
- `no dbt project at <root>: its dbt_project.yml is missing, is not
  YAML, or names no project`: the project's file no longer reads, as
  mid-edit; fix it, and the next request reads it again.
- `<file> lies in the dbt project at <other>, and this session serves
  only the one at <root>`: a session serves the first project it finds;
  for that file's answers, start a session in its project's directory.
  A nodeInfo at a position in that file gives it too.
- `dbt-lsp-plus does not answer textDocument/implementation`, or another
  method: no server of this session answers it; nothing to fix.
- Answers older than an `Edit`: an `Edit` of a file the server has not
  opened, other than the first, opens it without saving it.  The next
  hover, definition or references finds the file changed on disk,
  starts a parse and is answered from the last index meanwhile; ask
  again once the parse has run.

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

`dbt-lsp-plus mcp` serves node and lineage as MCP tools, each call
answered as the subcommand answers it from a shell, from the server's
directory ([`mcp [PATH]`](../reference/cli.md#mcp-path)).  Codex
declares it in `~/.codex/config.toml`:

```toml
[mcp_servers.dbt]
command = "dbt-lsp-plus"
args = ["mcp"]
tool_timeout_sec = 180
```

A call after a change to the project's files waits for a `dbt parse`: a
client whose tool timeout is 60 s by default, as Codex's, may give up on
it, hence the longer timeout above.  In Claude Code, beside the plugin:

```
claude mcp add dbt -- dbt-lsp-plus mcp
```

A harness with a setting for language servers takes the command and the
files above: `.sql` as `sql`, `.yml` and `.yaml` as `yaml`.  For the
language server's features, in a harness that takes MCP servers only, an
LSP-to-MCP bridge runs dbt-lsp-plus behind it.
