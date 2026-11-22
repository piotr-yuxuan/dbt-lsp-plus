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
has `lsp`, whether the proxy would answer alone and why, whether dbt can
be cut off the network, and why a parse would be refused
([the subcommands](../reference/cli.md)).  A given `--project-dir`
without `dbt_project.yml` is no project to doctor, where the server
takes the directory all the same, and dbt
fails there (its dbt lsp, else the proxy's parse).

## Who answers a request

- **dbt-lsp-plus** answers the outline, workspace symbols, the call
  hierarchy, folding, highlights, selection ranges, the rename of a CTE,
  inlay hints and `dbt-lsp-plus.nodeInfo`; references and renames on a
  source; definitions and references of the kinds of target in [Where
  dbt-lsp-plus answers definitions and
  references](use-with-an-agent.md#where-dbt-lsp-plus-answers-definitions-and-references).
  The answer to `initialize` names those kinds, per request, under
  `capabilities.experimental.dbtLspPlus`.  In the default mode, `merge`,
  with `dbt lsp` running, it asks `dbt lsp` there too, and `dbt lsp`'s
  locations that match none of its own follow them.  For each operation
  of an agent's LSP tool, see [Who answers
  what](use-with-an-agent.md#who-answers-what); for each method, [the
  modes](../reference/features.md#modes).
- **`dbt lsp`** answers everything else: the proxy forwards the request
  and the answer unchanged, byte for byte.  In the default mode, `merge`,
  a definition, references, hover or inlay hints that go to `dbt lsp`
  wait for it 300 ms where the proxy has something to add (its
  locations, a summary, hints), 2 s where it has nothing; past that, or
  when `dbt lsp` fails, exits or is not running, the proxy answers with
  its part, else null or `[]`, and logs once per method and deadline
  that `dbt lsp` was late (`window/logMessage`, type 4).
  Set a method to `dbt` (`--mode textDocument/hover=dbt`) to wait for
  `dbt lsp` however long it takes.  Any other slow answer is the
  server's.
- **Alone**, with dbt-oss, references at a position the proxy does not
  claim are `[]` and a definition there null; the proxy fails every other
  request it does not answer at once, with -32803, rather than leave it
  waiting.

## A request times out

A client gives up after a while: lsp-mode after `lsp-response-timeout`
seconds, 10 by default.

1. Find who was asked, from the method, its mode and the position: in
   `merge` with `dbt lsp` running, a definition, references, hover or
   inlay hints go to `dbt lsp`, at the kinds that
   `experimental.dbtLspPlus` lists too, and never wait for it more than
   2 s; in `plus`, `dbt lsp` is not asked for these methods at all.
2. `dbt lsp` answers once it has compiled the project.  A large project
   may take longer than the client's timeout to compile, after the server
   starts or the project changes; the server reports its progress, in
   percent, then sends `dbt/lspCompileComplete`.  Ask again after it, or
   raise the client's timeout.  A compilation with no end 60 s after it
   started is said once a session, `dbt lsp has been compiling for 60 s
   with no end yet (its log: …)`, and one that compiled nothing once for
   each first error, `dbt lsp compiled nothing: …`: see "Common
   problems".
3. Read the client's log of messages: whether the request went out,
   whether an answer came back, and when.
4. Start `dbt lsp` directly in place of the proxy, and ask again.  A
   request that times out there too is the server's; one that answers
   only without the proxy is a fault of dbt-lsp-plus: report it.

## Where the proxy says what went wrong

- **Its standard error**, for what happens before a client can be told:
  `dbt-lsp-plus: cannot start ...` when the server command does not run;
  that `unshare -rnm` cannot cut its `dbt parse` off the network here;
  without `--project-dir`, which project and which dbt it found, the line
  that `window/logMessage` repeats.
- **`window/logMessage`**, once per reason, each prefixed
  `dbt-lsp-plus:`: that the dbt has no `lsp` and the proxy answers alone;
  that `dbt lsp` ended and restarts in so many seconds, at most three
  times in ten minutes; why its `dbt parse` failed or was refused.
- **`window/showMessage`**, which an editor shows at once and Claude
  Code does not show at all: that a compilation of `dbt lsp`'s compiled
  nothing, with dbt's first error, once for each first error, and that
  one has no end 60 s after it started, once a session; each names `dbt
  lsp`'s own log, `logs/dbt-lsp.log` under the project.

## Common problems

- **No hover summary, inlay hints or node information.**  The proxy's `dbt
  parse` failed: most often dbt found no `profiles.yml`, which, with
  `--project-dir`, it looks for in the proxy's working directory and in
  `~/.dbt`, not in the project.  Pass `--profiles-dir` in the server
  command (see [the
  configuration](../reference/features.md#configuration)).
- **A hover is an error, "dbt parse has not finished: ...".**  The proxy
  answers the hover itself (alone, or in `plus`) and has no index yet:
  "it is running", ask again once the parse has ended; "waiting for dbt
  lsp to compile the project", ask again once `dbt lsp` has compiled;
  any other reason is the parse's failure, which the entry above, or the
  log's `dbt-lsp-plus: dbt parse failed: ...`, says how to fix.
- **Every answer null, and nothing in the log.**  Without `--project-dir`,
  neither the client's folder nor any file opened lies in a dbt project:
  open a model, or pass `--project-dir`.
- **Usage statistics sent with dbt 2.0.9.**  The default command, the
  Claude Code plugin's, is `dbt lsp` with no flag; pass
  `--no-send-anonymous-usage-stats` in the server command (see [Use it
  with an agent](use-with-an-agent.md)).
- **Not the dbt expected.**  The line `dbt-lsp-plus: project ..., dbt ...`
  says which it took and which it skipped, and why: a `.venv` whose `dbt`
  is dbt-core 1.x is skipped.  A path in the server command, in place of
  the bare `dbt`, is run as written.
- **"dbt parse would install it, rewriting the install path; run dbt
  deps".**  A package is missing, or the lock is older than the files that
  name it: the proxy will not let its parse install packages.  Run `dbt
  deps`.
- **"cannot read …/dbt_project.yml as YAML", and no parse, on macOS.**
  Where the proxy cannot cut its parse off the network, it refuses a
  project file that does not read as YAML, as dbt might still install
  packages from it.  Fix the file.
- **"has no lsp: dbt-lsp-plus answers alone".**  The dbt is dbt-oss: the
  proxy's own features work, formatting, completion, compiled SQL and
  previews do not (see [Use it with dbt-oss alone](use-with-dbt-oss.md)).
- **The server restarts, then the proxy stays alone.**  `dbt lsp` ended
  four times in ten minutes; its standard error says why.
- **"dbt lsp compiled nothing: …".**  `dbt lsp` compiled none of the
  project, so it has nothing to answer from (a listing answers
  `Compilation failed`); the text after the colon is dbt's first error,
  also published as a diagnostic, often on `dbt_project.yml` or on
  `<RUN_OPERATION>`, a file that does not exist.  Read
  `logs/dbt-lsp.log`, which the message names: a profile's field, or an
  `env_var` the editor's environment lacks (a GUI editor on macOS may
  not get the shell's); a hook (`on-run-start`) that fails; a package
  that is not installed; the warehouse's connection or authentication.
  Fix it and save a file: `dbt lsp` compiles again.  An error in one
  model is no such case: the other models compile, and the diagnostic on
  that model says it.
- **"dbt lsp has been compiling for 60 s with no end yet".**  `dbt lsp`
  started a compilation and has not finished it; a merged request gets
  the proxy's answer alone at its deadline (see "Who answers a request").
  Read `logs/dbt-lsp.log`: a first compilation of a large project on a
  warehouse can take that long, and its progress, in percent, keeps
  moving; a connection or an authentication that hangs does not.  The
  message comes once a session.

## Run it by hand

```sh
dbt-lsp-plus --version
dbt --version
dbt lsp --help            # fails on dbt-oss, which has no lsp
dbt parse --project-dir /path/to/project --profiles-dir /path/to/project
```

The last one shows the errors that the proxy's own parse meets.

## Report a problem

Open an [issue](https://github.com/piotr-yuxuan/dbt-lsp-plus/issues) with
the versions of dbt-lsp-plus, dbt and the editor, the server command, the
lines the proxy wrote (standard error and log messages), and the client's
log of the messages around the request.
