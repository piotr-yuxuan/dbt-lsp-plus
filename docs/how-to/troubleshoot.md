# Troubleshoot

In Emacs with dbt-mode, start with
[dbt-mode's troubleshooting](https://github.com/piotr-yuxuan/dbt-mode/blob/main/docs/how-to/troubleshoot.md),
which says where Emacs shows each of the logs below.  With an agent,
start with [Use it with an agent](use-with-an-agent.md): its check in
one call and [When nothing
answers](use-with-an-agent.md#when-nothing-answers).

First, from a shell, run `dbt-lsp-plus doctor PATH -- ARGUMENTS`, PATH a
file of the project and ARGUMENTS those the editor gives the server: it
says which project and which dbt the server would take, whether that dbt
answers as a dbt 2 does, and why a parse would be refused
([the subcommands](../reference/cli.md)).  A given `--project-dir`
without `dbt_project.yml` is no project to doctor, where the server
takes the directory all the same, and its parse fails there.

## Who answers a request

- **dbt-lsp-plus** answers every request of [its methods and
  commands](../reference/features.md#methods-and-commands): the outline,
  workspace symbols, the call hierarchy, folding, highlights, selection
  ranges, the rename of a CTE, hovers, inlay hints and
  `dbt-lsp-plus.nodeInfo`; references and renames on a source;
  definitions at any target, and references at any but a `doc()` or a
  generic test's name in YAML, where they are `[]`.  For each operation
  of an agent's LSP tool, see [Who answers
  what](use-with-an-agent.md#who-answers-what).
- At a position that names no target, references are `[]`, a definition
  and a rename null.
- **No one else**: a method or a command that dbt-lsp-plus does not
  answer, completion and formatting among them, gets error -32601
  `dbt-lsp-plus does not answer <name>` at once.

## A request times out

A client gives up after a while: lsp-mode after `lsp-response-timeout`
seconds, 10 by default.  dbt-lsp-plus answers each request at once, from
the files and the last parse's index; a request that reads the index
asks for a parse first when the index is stale, and answers from the
last one meanwhile, or says why there is none.

1. Read the client's log of messages: whether the request went out,
   whether an answer came back, and when.
2. Run `dbt-lsp-plus doctor` on the file: a dbt that does not run, or a
   parse that the guard on packages refuses, leaves the requests that
   read the index without one.
3. A request that times out with an answer on its way is a fault of
   dbt-lsp-plus: report it.

## Where the server says what went wrong

- **Its standard error**, for what happens before a client can be told:
  which project and which dbt it found, the line that
  `window/logMessage` repeats.
- **`window/logMessage`**, once per reason, each prefixed
  `dbt-lsp-plus:`: which project and which dbt it found; why its `dbt
  parse` failed or was refused; a problem of the parse in no file of the
  project.

## Common problems

- **No hover summary, inlay hints or node information.**  The server's
  `dbt parse` failed: most often dbt found no `profiles.yml`, which, with
  `--project-dir`, it looks for in the server's working directory and in
  `~/.dbt`, not in the project.  Pass `--profiles-dir` in the server
  command (see [the
  configuration](../reference/features.md#configuration)).
- **A hover is an error, "dbt parse has not finished: ...".**  There is
  no index yet: "it is running", ask again once the parse has ended; "it
  has not run, or left no manifest", ask again, then run doctor; any
  other reason is the parse's failure, which the entry above, or the
  log's `dbt-lsp-plus: dbt parse failed: ...`, says how to fix.
- **Every answer null, and nothing in the log.**  Without `--project-dir`,
  neither the client's folder nor any file opened lies in a dbt project:
  open a model, or pass `--project-dir`.
- **Not the dbt expected.**  The line `dbt-lsp-plus: project ..., dbt ...`
  says which it took and which it skipped, and why: a `.venv` whose `dbt`
  is dbt-core 1.x is skipped.  A path in the server command, in place of
  the bare `dbt`, is run as written.
- **"dbt parse would install it, rewriting the install path; run dbt
  deps".**  A package is missing, or the lock is older than the files that
  name it: the server will not let its parse install packages.  Run `dbt
  deps`.
- **"cannot read …/dbt_project.yml as YAML", and no parse.**  The server
  refuses a project file that its YAML reader cannot read, a tab after a
  key perhaps, which dbt may read and install packages from.  Fix the
  file.
- **"dbt-lsp-plus does not answer textDocument/completion".**  Completion,
  formatting and previews are none of dbt-lsp-plus's.

## With dbt Labs' build

dbt-lsp-plus runs no `dbt lsp`, whichever build the server command names:
its `lsp` word only names the user's dbt.  A user of dbt Labs' build who
wants what only `dbt lsp` gives, completion, formatting, its own compile
diagnostics or column-level features, may run `dbt lsp` beside
dbt-lsp-plus, as a second server of the editor's.  It then shows its own
diagnostics, under the source `dbt`, beside those of dbt-lsp-plus, and
writes in the project's `target/` and `logs/` at each compilation, its
log `logs/dbt-lsp.log` among them; dbt-lsp-plus starts no parse for a
change in either directory.

## Run it by hand

```sh
dbt-lsp-plus --version
dbt --version
dbt lsp --help            # dbt-oss refuses it, which is no failure
dbt parse --project-dir /path/to/project --profiles-dir /path/to/project
```

The last one shows the errors that the server's own parse meets.

## Report a problem

Open an [issue](https://github.com/piotr-yuxuan/dbt-lsp-plus/issues) with
the versions of dbt-lsp-plus, dbt and the editor, the server command, the
lines the server wrote (standard error and log messages), and the
client's log of the messages around the request.
