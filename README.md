# dbt-lsp-plus

A language server that sits in front of `dbt lsp`, the language server of
dbt 2, and adds what it lacks, for every editor and for agents.

[The features](docs/reference/features.md) are listed by LSP method.

It uses only the server's interface, unlocks nothing that dbt gates behind
an account, and does not ship dbt.  [The
design](docs/explanation/design.md) explains how it works and what it
would take to finish.

- [Configure an editor](docs/how-to/configure-an-editor.md)
- [Use it with dbt-oss alone](docs/how-to/use-with-dbt-oss.md)
- [Build and test](docs/how-to/build-and-test.md)
- [The command line](docs/reference/command-line.md)

## Licence

European Union Public Licence 1.2 (`LICENSE`) or GNU General Public
License 3.0 or later (`COPYING`).
