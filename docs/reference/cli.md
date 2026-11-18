# The subcommands

Beside the language server, `dbt-lsp-plus` answers two questions from a
shell, without an LSP client, each on one line of compact JSON on the
standard output:

```
dbt-lsp-plus doctor [PATH] [-- ARGUMENTS...]
dbt-lsp-plus select SELECTOR [-- ARGUMENTS...]
```

A subcommand is the first word after the program's name, and nothing
else: `dbt-lsp-plus -- doctor` and `dbt-lsp-plus --shadow select x` still
start a server whose command is `doctor` or `select x`.  A server command
whose first word is a subcommand's name follows `--`.

ARGUMENTS are the server's own command line, as an editor or `.lsp.json`'s
`args` give it: `--backend never`, then `--` and the server command,
`dbt lsp` by default (`-- dbt lsp --target prod`, `-- uvx dbt lsp`,
`-- dbt lsp --project-dir dbt`).  Of them the subcommands read the server
command and `--backend`; `--mode`, `--default-mode` and `--shadow` are
checked and unused.

`-h` or `--help` right after the subcommand's name prints its usage.

## The project and its dbt

Both subcommands find the project and its dbt exactly as the server
does, from a path, doctor's PATH, else the current directory:

- **given**: the server command names `--project-dir`; the project is
  that directory, made absolute, and the dbt is the server command as
  written, run in the current directory;
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
{"alone":"dbt lsp is not started for a project found from a file",
 "cut":true,
 "dbt":{"command":["/w/repo/dbt/.venv/bin/dbt","lsp"],"lsp":true,
        "named":"/w/repo/dbt/.venv/bin/dbt","skipped":[],
        "source":"the project's .venv","why":null},
 "packages":null,"path":"/w/repo/dbt/models/marts/orders.sql",
 "project":"/w/repo/dbt","route":"document","version":"0.1.9"}
```

(shown folded; printed on one line, its keys sorted.)

| Key | Value |
|-----|-------|
| `version` | the version of dbt-lsp-plus |
| `path` | PATH, made absolute from the current directory |
| `project` | the project's directory |
| `route` | `given`, `folder` or `document`, as above |
| `dbt.command` | the server command that would run, its dbt in place of a bare `dbt` |
| `dbt.named` | the dbt: its path, or the words of the server command before `lsp` |
| `dbt.source` | where it was found: `the server command`, `the project's .venv`, `the repository's .venv`, `the main worktree's project .venv`, `the main worktree's .venv` or `PATH` |
| `dbt.skipped` | each place passed over, as `{"path": ..., "why": ...}`, such as a dbt-core 1.x whose `lsp --help` gives exit status 2 |
| `dbt.lsp` | `true` when the dbt has `lsp`, `false` when it is a dbt 2 without, as dbt-oss; `null` when it does not answer as a dbt 2 does |
| `dbt.why` | when `dbt.lsp` is `null`, how the dbt answered (`cannot run: ...`, `lsp --help gives exit status: 1`, no answer within 10 s); else `null` |
| `alone` | `null` when `dbt lsp` would start behind the proxy; else why the proxy would answer alone: `the backend is never`, `dbt lsp is not started for a project found from a file` or `it has no lsp` |
| `cut` | whether this system can cut dbt off the network (`unshare -rnm`) |
| `packages` | `null`, or why the proxy's first `dbt parse` would be refused: packages that dbt would install first, or that the network would be needed for |

Without a project, doctor runs nothing at all and says so:

```json
{"error":"no dbt_project.yml at or above /w/notes/x.sql; give a path within the project",
 "path":"/w/notes/x.sql","project":null,"version":"0.1.9"}
```

Exit status: 0 when a project is found whose dbt answers as a dbt 2 does
(`dbt.lsp` `true` or `false`); 1 otherwise; 2 for a usage error.

What it runs is what the server runs as it starts, and nothing else:
`git rev-parse` once, on the folder's and a document's routes; `lsp
--help` of each dbt it looks at, the dbt that the server would leave
unprobed included (the one on `PATH`, a first word of the user's own, the
command as written when given); and `unshare -rnm` on `true`.  None of
these is cut off the network, as the server's are not: a launcher such as
`uvx dbt` may reach it while it answers `lsp --help`.  doctor parses
nothing and writes nothing.

Left out: the profile, the target and the environment variables the
profile needs, which `dbt debug` would tell by connecting to the
warehouse; and the state of a running server's index.

## `select SELECTOR`

The nodes that the dbt selector SELECTOR selects in the project of the
current directory: `dbt list --select=SELECTOR --output json`, run cut off
the network as the proxy's own runs are, in a run directory under the
user's cache (`$XDG_CACHE_HOME`, else `~/.cache`, else the system's
temporary directory) that it removes once it has the answer.  The answer
is what the command `dbt-lsp-plus.nodes` gives a client:

```
{"error":null,"nodes":[{"unique_id":"model.shop.orders","depends_on":{"nodes":["model.shop.stg_orders"]},...}]}
```

each node a line of `dbt list`'s output as dbt printed it.  On a failure,
`nodes` is `[]` and `error` says why:

- `No nodes found`, when the selector selects nothing;
- `dbt-lsp-plus runs dbt only cut off the network, which this system
  cannot`, where `unshare -rnm` cannot run, as on macOS, checked first:
  nothing else runs, neither git nor a dbt's probe;
- `no dbt_project.yml at or above DIR; run it within the project, or
  name it with -- dbt lsp --project-dir DIR`, outside every project:
  nothing runs;
- why a parse would be refused, as doctor's `packages`, checked before
  the dbt is looked for: nothing runs;
- the first error dbt logged, or how dbt ended.

Exit status: 0 when `error` is `null`; 1 otherwise, `No nodes found`
included, as `grep` finding nothing; 2 for a usage error.

Fields, limits and paging, and an answer from the last parse's manifest
without running dbt, are not there yet.

## The standard error

Nothing is written there but a usage error, which ends with status 2 and
the subcommand's usage; the answer is always the one line of the standard
output, with `error` when something is missing or failed.
