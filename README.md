# dbt-lsp-plus

[![CI](https://github.com/piotr-yuxuan/dbt-lsp-plus/actions/workflows/ci.yml/badge.svg)](https://github.com/piotr-yuxuan/dbt-lsp-plus/actions/workflows/ci.yml)
[![CD](https://github.com/piotr-yuxuan/dbt-lsp-plus/actions/workflows/cd.yml/badge.svg)](https://github.com/piotr-yuxuan/dbt-lsp-plus/actions/workflows/cd.yml)
[![Weekly](https://github.com/piotr-yuxuan/dbt-lsp-plus/actions/workflows/weekly.yml/badge.svg)](https://github.com/piotr-yuxuan/dbt-lsp-plus/actions/workflows/weekly.yml)
[![Latest release](https://img.shields.io/github/v/release/piotr-yuxuan/dbt-lsp-plus?sort=semver)](https://github.com/piotr-yuxuan/dbt-lsp-plus/releases/latest)
[![Licence](https://img.shields.io/github/license/piotr-yuxuan/dbt-lsp-plus)](https://github.com/piotr-yuxuan/dbt-lsp-plus/blob/main/LICENSE)
[![Issues](https://img.shields.io/github/issues/piotr-yuxuan/dbt-lsp-plus)](https://github.com/piotr-yuxuan/dbt-lsp-plus/issues)
[![Minimum Rust](https://img.shields.io/badge/rust-1.89%2B-informational)](https://github.com/piotr-yuxuan/dbt-lsp-plus/blob/main/Cargo.toml)

A language server that sits in front of `dbt lsp`, the language server
of dbt 2, and adds what it lacks, for every editor and for agents: the
outline of a model, every resource of a project as a workspace symbol,
the call hierarchy of models, sources, macros and functions, references
and renames of sources, definitions from YAML (and alone, of refs,
macros and functions), and the last run's failures as diagnostics.  With
dbt-oss, which has no `dbt lsp`, it works alone, from the files and your
own `dbt parse`.  It uses only the server's interface, unlocks nothing
that dbt gates behind an account, and does not ship dbt.  It is
pre-release.

![](./docs/social-media-preview.png)

## Install and run

With Rust 1.89 or later and a C linker (or `nix develop`, which gives
Rust):

```
git clone https://github.com/piotr-yuxuan/dbt-lsp-plus
cd dbt-lsp-plus
cargo install --locked --path . --bin dbt-lsp-plus
```

Then have your editor's LSP client start, in place of `dbt lsp`:

```
dbt-lsp-plus -- dbt lsp --project-dir /path/to/project
```

The proxy answers what the server lacks, and passes every other message
unchanged, byte for byte.  [Your first
session](docs/tutorials/first-session.md) walks through it on the project
of the tests, from Emacs.

Without `--project-dir`, the project is the client's folder when it holds
`dbt_project.yml`, else the one of the first file opened in a project, a
nested `dbt/` or a worktree; and `dbt` is the project's `.venv`'s, the
repository's or the main worktree's, else the one on `PATH`.  Outside a
dbt project it stays quiet.

For Claude Code, the repository is a plugin marketplace whose plugin
starts `dbt-lsp-plus` from `PATH` for `.sql`, `.yml` and `.yaml` files:

```
claude plugin install dbt-lsp-plus --marketplace piotr-yuxuan/dbt-lsp-plus
```

[Use it with an agent](docs/how-to/use-with-an-agent.md) says more.

## Documentation

- Tutorial: [Your first session](docs/tutorials/first-session.md)
- How-to: [Configure an editor](docs/how-to/configure-an-editor.md),
  [Use it with an agent](docs/how-to/use-with-an-agent.md),
  [Use it with dbt-oss alone](docs/how-to/use-with-dbt-oss.md),
  [Build and test](docs/how-to/build-and-test.md),
  [Troubleshoot](docs/how-to/troubleshoot.md)
- Reference: [The features](docs/reference/features.md), by LSP method,
  then [the configuration](docs/reference/features.md#configuration), every option;
  [The release pipeline](docs/reference/release-pipeline.md)
- Explanation: [The design](docs/explanation/design.md), what `dbt lsp`
  was observed to do, and what it would take to finish

## Licence

European Union Public Licence 1.2 ([`LICENSE`](LICENSE)) or GNU General
Public License 3.0 or later ([`COPYING`](COPYING)).
