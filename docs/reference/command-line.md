# Command line

```
dbt-lsp-plus [--] [SERVER COMMAND...]
dbt-lsp-plus -h | --help
dbt-lsp-plus -V | --version
```

The proxy speaks LSP on its standard input and output, and runs SERVER
COMMAND behind it.  The server command defaults to `dbt lsp`.  The project
is the `--project-dir` of the server command, else the client's root
folder, else the current directory.  For example:

```
dbt-lsp-plus -- dbt lsp --project-dir /path/to/project
```

| Argument | Meaning |
|----------|---------|
| `--` | Ends the proxy's own arguments: every word after it is the server command |
| `-h`, `--help` | Prints the usage and exits |
| `-V`, `--version` | Prints the version and exits |

`-h`, `-V` and `--` are read only as the first argument.  Before it spawns
the server, the proxy runs the words of its command before `lsp`, then
`lsp --help`; with a dbt that has no `lsp`, it answers alone (see
[Use it with dbt-oss alone](../how-to/use-with-dbt-oss.md)).
