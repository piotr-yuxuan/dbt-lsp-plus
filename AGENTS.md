# Working in this repository

dbt-lsp-plus is a proxy in front of `dbt lsp` that adds LSP features the
server lacks.  `README.md` describes it for users; `docs/design.md` explains
its design, what `dbt lsp` was observed to do, and what is left to do.

## Workflow

Every change follows the skill `workflow:plans`; load it before the first
edit.  In this repository: plans are `docs/work/plans/NNN-slug.md`, not
tracked; the checks, run in `nix develop`, are:

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

and, before a merge, the live test with `DBT_LSP_PLUS_TEST_DBT` naming a
dbt 2 executable:

```
DBT_LSP_PLUS_TEST_DBT=/path/to/dbt cargo test --test live -- --ignored
```

## Layout

```
src/rpc.rs                    framing; frames keep their bytes
src/text.rs                   LSP positions, encodings, incremental changes
src/scan.rs                   one file of SQL and Jinja: blocks, CTEs, calls
src/yaml.rs                   YAML with the place of each scalar
src/project.rs                the resources of a project and its packages
src/graph.rs                  the dependency graph: scanned, then the server's
src/features.rs               the answers of the added features, as LSP JSON
src/proxy.rs                  the event loop, the server process, the listing
tests/proxy.rs                the binary in front of the fake server
tests/live.rs                 the binary in front of the real dbt lsp, opt-in
tests/support/                the test client and fake-dbt-lsp
tests/fixtures/shop/          the fixture dbt project, on DuckDB
```

## Rules of the code

- Forward what the proxy does not own byte for byte; never re-serialise it.
- Own a method only when the server does not announce it.
- The proxy's own requests carry ids `dbt-lsp-plus/N`, and their answers
  never reach the client.
- What `dbt lsp` does is observed, not documented: record each new
  observation in `docs/design.md`, make the fake server behave the same,
  and let the live test check it.
