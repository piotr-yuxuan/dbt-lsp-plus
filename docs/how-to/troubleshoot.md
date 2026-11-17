# Troubleshoot

In Emacs with dbt-mode, start with
[dbt-mode's troubleshooting](https://github.com/piotr-yuxuan/dbt-mode/blob/main/docs/how-to/troubleshoot.md),
which says where Emacs shows each of the logs below.

## Who answers a request

- **dbt-lsp-plus** answers the outline, workspace symbols, the call
  hierarchy, folding, highlights, selection ranges, the rename of a CTE,
  inlay hints and `dbt-lsp-plus.nodeInfo`; references and renames on a
  source; definitions of the kinds of target it lists.  The answer to
  `initialize` names those kinds, per request, under
  `capabilities.experimental.dbtLspPlus`.
- **`dbt lsp`** answers everything else, the references of a model or a
  macro included: the proxy forwards the request and the answer
  unchanged, byte for byte.  A slow answer there is the server's.
- **Alone**, with dbt-oss, the proxy fails every request it does not
  answer at once, with -32803, rather than leave it waiting.

## A request times out

A client gives up after a while: lsp-mode after `lsp-response-timeout`
seconds, 10 by default.

1. Find who was asked, from the position (a source or not) and the list
   in `experimental.dbtLspPlus`.
2. `dbt lsp` answers once it has compiled the project.  A large project
   may take longer than the client's timeout to compile, after the server
   starts or the project changes; the server reports its progress, then
   sends `dbt/lspCompileComplete`.  Ask again after it, or raise the
   client's timeout.
3. Read the client's log of messages: whether the request went out,
   whether an answer came back, and when.
4. Start `dbt lsp` directly in place of the proxy, and ask again.  A
   request that times out there too is the server's; one that answers
   only without the proxy is a fault of dbt-lsp-plus: report it.

## Where the proxy says what went wrong

- **Its standard error**, for what happens before a client can be told:
  `dbt-lsp-plus: cannot start ...` when the server command does not run;
  that `unshare -rnm` cannot cut its `dbt parse` off the network here.
- **`window/logMessage`**, once per reason, each prefixed
  `dbt-lsp-plus:`: that the dbt has no `lsp` and the proxy answers alone;
  that `dbt lsp` ended and restarts in so many seconds, at most three
  times in ten minutes; why its `dbt parse` failed or was refused.

## Common problems

- **No hover summary, inlay hints or node information.**  The proxy's `dbt
  parse` failed: most often dbt found no `profiles.yml`, which it looks for
  in the proxy's working directory and in `~/.dbt`, not in the project.
  Pass `--profiles-dir` in the server command (see [the
  configuration](../reference/features.md#configuration)).
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
