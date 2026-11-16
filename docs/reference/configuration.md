# Configuration

dbt-lsp-plus reads no configuration file and no LSP setting: everything
it takes is on its command line, which ends with the server command, and
in its environment.  Every option at once, for a project whose profile
sits beside it:

```sh
env DBT_PACKAGES_INSTALL_PATH=vendor/dbt_packages \
    DBT_TARGET_PATH=target \
    XDG_CACHE_HOME="$HOME/.cache" \
  dbt-lsp-plus -- \
    dbt lsp \
      --project-dir /path/to/project \
      --profiles-dir /path/to/project \
      --profile shop \
      --target dev \
      --vars '{"start_date": "2024-01-01"}' \
      --packages-install-path vendor/dbt_packages
```

Most setups need far less: `dbt-lsp-plus -- dbt lsp --project-dir
/path/to/project`, or nothing after `dbt-lsp-plus` when the editor starts
it in the project.

## The proxy's arguments

```
dbt-lsp-plus [--] [SERVER COMMAND...]
dbt-lsp-plus -h | --help
dbt-lsp-plus -V | --version
```

They are read only as the first argument.

- `--`: ends the proxy's own arguments; every word after it is the
  server command.  Optional, unless the server command starts with `-h`,
  `--help`, `-V` or `--version`.
- `-h`, `--help`: prints the usage and exits.
- `-V`, `--version`: prints `dbt-lsp-plus X.Y.Z` and exits.

## The server command

- `SERVER COMMAND...`: the command that runs the language server behind
  the proxy.  Default: `dbt lsp`.  The proxy speaks LSP on its standard
  input and output, and runs this command with the same streams behind
  it.
- The words before `lsp` are how dbt is launched: `dbt`, a path to a dbt
  executable, `uvx dbt`, or `env X=1 dbt`.  Before it spawns the server,
  the proxy runs these words then `lsp --help`, for at most 10 s.  If dbt
  answers that it has no `lsp` subcommand, as dbt-oss does, the proxy
  answers alone, from the files and its own `dbt parse` (see [Use it with
  dbt-oss alone](../how-to/use-with-dbt-oss.md)).
- A server command without the word `lsp` is run as it is, as a wrapper
  of the user's own: the proxy then runs no `dbt parse`.

## The flags its `dbt parse` keeps

The proxy runs its own `dbt parse` for what it answers from the manifest.
It builds that command from the server command: the same launcher, then
these flags when the server command has them, as `--flag X` or
`--flag=X`.  `dbt lsp` reads them too.

- `--project-dir DIR`: the dbt project.  Default: the client's first
  workspace folder or `rootUri`, when it holds a `dbt_project.yml`; else
  the proxy's working directory.  A relative `DIR` is taken from the
  proxy's working directory.
- `--profiles-dir DIR`: where `profiles.yml` is.  Default, as dbt's: the
  proxy's working directory, then `~/.dbt`, not the project.  An editor
  that starts the proxy in a subdirectory of the project needs it.
- `--profile NAME`: the profile.  Default: the `profile` of
  `dbt_project.yml`.
- `--target NAME`: the target of the profile.  Default: the profile's
  `target`.
- `--vars YAML`: the project's variables, as a YAML or JSON mapping.
  Default: none.
- `--packages-install-path DIR`: where the packages are installed.
  Default: `DBT_PACKAGES_INSTALL_PATH`, else the
  `packages-install-path` of `dbt_project.yml`, else `dbt_packages`.

Every other flag goes to `dbt lsp` alone.  The parse also gets
`--target-path` and `--log-path` in the proxy's cache, `--write-json`,
`--log-format json`, `--quiet`, `--no-version-check`,
`--no-send-anonymous-usage-stats` and `--no-manage-state`; none of these
can be changed.

## The environment

- `DBT_PACKAGES_INSTALL_PATH`: where the packages are installed, below
  `--packages-install-path` and above `dbt_project.yml`.  Read from the
  proxy's environment, or from a word `DBT_PACKAGES_INSTALL_PATH=DIR`
  before `lsp` in the server command, which wins.  An empty value counts
  as unset.
- `DBT_TARGET_PATH`: where the proxy watches the user's last run
  (`run_results.json`, `sources.json` and `manifest.json`), relative to
  the project.  Default: `target`.
- `XDG_CACHE_HOME`: where the proxy keeps its parses, under
  `dbt-lsp-plus/`.  Default: `$HOME/.cache`, else the system's temporary
  directory.  A relative value counts as unset.  A cache that no parse
  wrote for a day is removed.

The server command, `dbt lsp` and the proxy's `dbt parse` inherit the
whole environment.

## Fixed limits

These are not configurable.

- A `dbt parse` is stopped after 120 s.
- The probe of `lsp --help` is stopped after 10 s.
- `workspace/symbol` answers at most 20 000 symbols.
- A message larger than 64 MiB stops the session.
- On Linux, the proxy's `dbt parse` runs under `unshare -rnm`, cut off the
  network with the project read-only, when the system allows it; else
  dbt's flags and the guard on packages keep it offline and the project
  as it is.
