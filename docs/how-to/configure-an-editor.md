# Configure an editor

dbt-lsp-plus is a language server on the standard input and output: any
client that starts a stdio server runs it in place of `dbt lsp`, with the
server command after `--`:

```
dbt-lsp-plus -- dbt lsp --project-dir /path/to/project
```

The project is the `--project-dir` of the server command, else the
client's root folder, else the current directory; [the
configuration](../reference/configuration.md) gives every option.  Alone, with
dbt-oss, the proxy's `dbt parse` keeps `--profiles-dir`, `--profile`,
`--target`, `--vars` and `--packages-install-path` from the server
command: without `--profiles-dir`, dbt looks for `profiles.yml` in the
proxy's working directory, then in `~/.dbt`, not in the project.

## Emacs with dbt-mode

dbt-mode's lsp-mode client runs `dbt lsp` through dbt-lsp-plus when
`dbt-lsp-proxy` is non-nil and `dbt-lsp-plus` is on `exec-path`, and runs
the proxy alone for dbt-oss.  `dbt-lsp-arguments` adds flags to the server
command:

```elisp
(setq dbt-lsp-proxy t
      dbt-lsp-arguments '("--profiles-dir" "/path/to/project"))
```

## Emacs with Eglot

```elisp
(add-to-list 'eglot-server-programs
             '(sql-mode . ("dbt-lsp-plus" "--" "dbt" "lsp"
                           "--project-dir" "/path/to/project")))
```

Then `M-x eglot` in a model.  Eglot gives the outline to `imenu`, and, from
Eglot 1.19, which Emacs 31 bundles, the call hierarchy to
`eglot-show-call-hierarchy`;
[Your first session](../tutorials/first-session.md) walks through both.
