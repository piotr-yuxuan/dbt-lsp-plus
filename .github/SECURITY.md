# Security policy

## What dbt-lsp-plus does

dbt-lsp-plus opens no network connection of its own. It reads the files of the project it
serves, runs the `dbt lsp` command that the user gives it as given, and runs the user's own
`dbt parse` with `--no-version-check --no-send-anonymous-usage-stats --no-manage-state`, its
output in a cache of its own. It ships no part of dbt.

## Existing tooling

GitHub Actions runs [every check](../docs/reference/release-pipeline.md) on every push, the
conformance corpus included, whose parses of dbt run with the network cut off. No
vulnerability scanner runs on this repository.

## Supported versions

Only the latest release is supported. The project is pre-release, so a fix reaches no
earlier version.

## Reporting a vulnerability

Report it privately through GitHub's private vulnerability reporting, under the
repository's **Security** tab, or open an issue for anything that is not sensitive.
