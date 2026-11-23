# Configure an editor

## Emacs with dbt-mode

[dbt-mode](https://github.com/piotr-yuxuan/dbt-mode) is the client to use:
a major mode for the SQL and Jinja of dbt projects, whose lsp-mode client
runs dbt-lsp-plus.  With dbt-lsp-plus on `exec-path`:

```elisp
(use-package dbt-mode
  :straight (dbt-mode :type git :host github :repo "piotr-yuxuan/dbt-mode")
  :hook ((dbt-mode . dbt-lsp-deferred)
         ((yaml-mode yaml-ts-mode) . dbt-properties-mode-maybe))
  :custom
  (dbt-lsp-proxy t)
  ;; Flags of the server command, here where dbt finds profiles.yml.
  (dbt-lsp-arguments '("--profiles-dir" "/path/to/project")))
```

dbt-mode steps aside for each feature the server says it answers, and
keeps its own navigation, previews and lineage, which need no server.
[Its documentation](https://github.com/piotr-yuxuan/dbt-mode#readme)
covers the rest.

## Any other editor

dbt-lsp-plus is a language server on the standard input and output: any
client that starts a stdio server runs it, with the server command after
`--`, which names your dbt and the flags of its parse:

```
dbt-lsp-plus -- dbt lsp --project-dir /path/to/project
```

The project is the `--project-dir` of the server command, else the
client's root folder when it holds `dbt_project.yml`, else the project of
the first file opened in one; the current directory names none.  Without
`--project-dir`, the bare `dbt` is the project's `.venv`'s, the
repository's or the main worktree's when one answers as dbt 2, else the
one on `PATH`, and dbt runs in the project; [the
configuration](../reference/features.md#configuration) gives every option.
Its `dbt parse` keeps `--profiles-dir`, `--profile`, `--target`, `--vars`
and `--packages-install-path` from the server command: with
`--project-dir` and without `--profiles-dir`, dbt looks for
`profiles.yml` in the server's working directory, then in `~/.dbt`, not
in the project.

## With either build of dbt 2

dbt-oss and dbt Labs' build alike: everything in [the
features](../reference/features.md) answers from the files, plus, from
your own `dbt parse`, run at each save or change of the project, node
information, the hover summary, inlay hints, the call hierarchy as dbt
resolves it, and dbt's errors and warnings as diagnostics (source `dbt
parse`).  Go to definition and references cover a `ref` and a macro call
in SQL, a `ref` in a Python model, a `function()` call in SQL or YAML,
and a macro call in YAML: across the project, versions included.
Compiled SQL comes from `dbt-lsp-plus.compile`, by dbt's CLI, refused
where a package may have on-run hooks.  Formatting, completion and
previews are none of its own.  The server says once in the log which
project and which dbt it found, and asks `lsp --help` only of a `dbt` it
looks for, to tell a dbt 2 from anything else.

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
