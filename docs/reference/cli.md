# The subcommands

Beside the language server, `dbt-lsp-plus` answers three questions from
a shell, without an LSP client, each on one line of compact JSON on the
standard output:

```
dbt-lsp-plus doctor [PATH] [-- ARGUMENTS...]
dbt-lsp-plus select SELECTOR [-- ARGUMENTS...]
dbt-lsp-plus node ID|PATH[:LINE[:COLUMN]] [-- ARGUMENTS...]
```

A subcommand is the first word after the program's name, and nothing
else: `dbt-lsp-plus -- doctor` and `dbt-lsp-plus --default-mode plus
select x` still start a server whose command is `doctor` or `select x`.  A server
command whose first word is a subcommand's name (`doctor`, `select` or
`node`) follows `--`.

ARGUMENTS are the server's own command line, as an editor or `.lsp.json`'s
`args` give it: `--` and the server command, `dbt lsp` by default
(`-- dbt lsp --target prod`, `-- uvx dbt lsp`, `-- dbt lsp --project-dir
dbt`).  Of them the subcommands read the server command; `--mode` and
`--default-mode` are checked and unused.

`-h` or `--help` right after the subcommand's name prints its usage, whatever
follows; elsewhere it is a usage error.

## The project and its dbt

Each subcommand finds the project and its dbt as the server does, from
a path: doctor's PATH, node's PATH, else the current directory:

- **given**: the server command names `--project-dir`; the project is
  that directory, made absolute, when it holds `dbt_project.yml`, and
  the dbt is the server command as written, run in the current
  directory.  Else the subcommands say there is no project, where the
  server takes the directory all the same, and its parse fails there;
- **folder**: the path is a directory that holds `dbt_project.yml`, as a
  client's folder; the dbt is found as the server finds it (the project's
  `.venv`, its repository's, its main worktree's, else `PATH`; see
  [Use it with an agent](../how-to/use-with-an-agent.md)), from the server
  command anchored in the current directory, and runs in the project;
- **document**: else the project of the path, the nearest directory
  above it that holds `dbt_project.yml` and lies in no package of one
  above; the dbt as for the folder.

## `doctor [PATH]`

What the server would find from PATH, `.` by default, and whether it
would work.  For a project found:

```json
{"dbt":{"command":["/w/repo/dbt/.venv/bin/dbt","lsp"],
        "named":"/w/repo/dbt/.venv/bin/dbt","skipped":[],
        "source":"the project's .venv","why":null},
 "packages":null,"path":"/w/repo/dbt/models/marts/orders.sql",
 "project":"/w/repo/dbt","route":"document","version":"0.1.9"}
```

(shown folded; printed on one line, its keys sorted.)

| Key | Value |
|-----|-------|
| `version` | the version of dbt-lsp-plus |
| `path` | PATH, made absolute from the current directory, each `..` dropping the component before it, as written |
| `project` | the project's directory |
| `route` | `given`, `folder` or `document`, as above |
| `dbt.command` | the server command that would run, a `.venv`'s dbt in place of a bare `dbt`; a bare `dbt` stays for `PATH` |
| `dbt.named` | the dbt: its path, or the words of the server command before `lsp` |
| `dbt.source` | where it was found: `the server command`, `the project's .venv`, `the repository's .venv`, `the main worktree's project .venv`, `the main worktree's .venv` or `PATH` |
| `dbt.skipped` | each place passed over, as `{"path": ..., "why": ...}`, such as a dbt-core 1.x whose `lsp --help` gives exit status 2 |
| `dbt.why` | when the dbt does not answer `lsp --help` as a dbt 2 does, with its help or with dbt-oss's refusal of `lsp`, how it answered (`cannot run: ...`, `lsp --help gives exit status: 1`, no answer within 10 s); else `null` |
| `packages` | `null`, or why the server's first `dbt parse` would be refused, alike on every route: packages that dbt would install first, or a file naming them that dbt-lsp-plus cannot read |

Without a project, doctor runs nothing at all and says so:

```json
{"error":"no dbt_project.yml at or above /w/notes/x.sql; give a path within the project",
 "path":"/w/notes/x.sql","project":null,"version":"0.1.9"}
```

On the given route the error is `no dbt_project.yml in DIR, the server
command's --project-dir`.

Exit status: 0 when a project is found whose dbt answers `lsp --help` as
a dbt 2 does (`dbt.why` `null`); 1 otherwise; 2 for a usage error.

What it runs is what the server runs as it starts, and nothing else:
`git rev-parse` once, on the folder's and a document's routes when the
server command's first word is `dbt`; `lsp --help` of each dbt it looks
at, the dbt that the server would leave unprobed included (the one on `PATH`, a first word of the user's own, the
command as written when given).  A launcher such as `uvx dbt` may
reach the network while it answers `lsp --help`.  doctor parses nothing
and writes nothing.

Left out: the profile, the target and the environment variables the
profile needs, which `dbt debug` would tell by connecting to the
warehouse; and the state of a running server's index.

## `select SELECTOR`

The nodes that the dbt selector SELECTOR selects in the project of the
current directory: `dbt list --select=SELECTOR --output json`, run as
the server's own runs are, in a run directory under the
user's cache (`$XDG_CACHE_HOME`, else `~/.cache`, else the system's
temporary directory) that it removes once it has the answer.  A select
killed by a signal leaves its run directory, and, killed with SIGKILL,
leaves dbt running on, unwatched; the next select or server on the
same project, as spelt, and server command removes such directories
once neither a parse nor a run has
written them for a day.  The answer
is what the command `dbt-lsp-plus.nodes` gives a client:

```
{"error":null,"nodes":[{"unique_id":"model.shop.orders","depends_on":{"nodes":["model.shop.stg_orders"]},...}]}
```

each node a line of `dbt list`'s output as dbt printed it.  On a failure,
`nodes` is `[]` and `error` says why:

- `No nodes found`, when the selector selects nothing;
- `no dbt_project.yml at or above DIR; run it within the project, or
  name it with -- dbt lsp --project-dir DIR`, outside every project:
  nothing runs;
- `no dbt_project.yml in DIR, the server command's --project-dir`, on
  the given route: nothing runs;
- why a parse would be refused, as doctor's `packages`, checked before
  the dbt is looked for: nothing runs;
- the first error dbt logged, or how dbt ended.

Exit status: 0 when `error` is `null`; 1 otherwise, `No nodes found`
included, as `grep` finding nothing; 2 for a usage error.

Fields, limits and paging, and an answer from the last parse's manifest
without running dbt, are not there yet.

## `node ID|PATH[:LINE[:COLUMN]]`

The node that a unique id names in the project of the current directory,
or that a place in a file names in the project of that file, as the
command `dbt-lsp-plus.nodeInfo` gives a client the node it names by an
id or by a position:

```
{"error":null,"node":{"unique_id":"model.shop.stg_orders","children":["model.shop.orders"],"summary":"model view, \"shop\".\"main\".\"stg_orders\"",...}}
```

`node` is the manifest's node, with `relation`, `tests`, `summary`, the
text of the summary a hover shows first, and `children`, the unique ids,
sorted, of the enabled nodes whose `depends_on.nodes` names it, every
test aside; exposures, metrics, semantic models, saved queries and unit
tests are not among them.

The operand is a place when a file exists at its PATH: tried as
`PATH:LINE:COLUMN`, `PATH:LINE`, then `PATH`, LINE and COLUMN from 1,
COLUMN in characters, PATH from the current directory, each `..`
dropping the component before it, as written.  A place names the `ref`
or `source` under it, or the entry whose `name:` is there; `PATH` alone
is `PATH:1:1`, which, where it names nothing else, stands for the one
node its file defines, but in a YAML file.  Any
other operand is a unique id, as written: `model.shop.orders`,
`source.shop.raw.customers`.  So is a PATH mistyped, or of a file not
written yet: within a project it gets `<operand> names no node`, after
a parse when no manifest is kept, and outside one the error of no
project.

The nodes are those of a `dbt parse` of the project, run as the server
runs its own (the same flags, the same guard on packages), in a run
directory under the user's cache that is removed once the answer is
given.  A parse that does not fail leaves its manifest in `kept/`,
beside the run directories of the same project, as spelt, and server
command; the next `node` on that project reads it, and runs no parse,
while every file of the project that may hold a resource, and every
package, stays as it was when that parse started: a file written,
added or removed since, even during the parse, makes the next `node`
parse again.  `kept/` holds one manifest, the last kept; the next
`select`, `node` or server on the project removes it once it has gone a
day unwritten.  The kept manifest does not see what changes outside
those files: `profiles.yml`, a `.env`, the environment, or a dbt
replaced in place.

On a failure, `node` is `null`, or the node of the last manifest a
failed parse still wrote, and `error` says why:

- `<ID> names no node`, when the index holds no such node, or the place
  names none; in its place, the failure of the last parse when that
  parse failed but still wrote a manifest;
- `dbt parse failed: <dbt's first error> (see <log>)`, the log gone
  with the run directory;
- `dbt parse failed: it left no manifest that reads`, when dbt ended
  well but no manifest could be read where it was told to write one, as
  under a wrapper that drops `--target-path`;
- why the parse was refused, as doctor's `packages`, or `the server
  command has no lsp, so dbt-lsp-plus runs no dbt parse`;
- `no dbt project at <root>: ...`, at a `ref`, a `source` or an entry's
  `name:` over a project whose `dbt_project.yml` does not read, once a
  failed parse still wrote a manifest;
- `<file> lies in the dbt project at <other>, and this session serves
  only the one at <root>`, at a place in another project than the server
  command's `--project-dir`;
- `no dbt_project.yml at or above DIR; give a PATH within one, run it
  within one, or name it with -- dbt lsp --project-dir DIR`, outside
  every project: nothing runs;
- `no dbt_project.yml in DIR, the server command's --project-dir`, on
  the given route: nothing runs.

Exit status: 0 when `error` is `null`; 1 otherwise; 2 for a usage error.
The answer has no time and no parse's id: the manifest kept is that of
the files as they lie.

## The standard error

Nothing is written there but a usage error, which ends with status 2 and
the subcommand's usage; the answer is always the one line of the standard
output, with `error` when something is missing or failed.
