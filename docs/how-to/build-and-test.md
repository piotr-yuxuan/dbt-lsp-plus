# Build and test

```
nix develop
cargo build --release
cargo test
DBT_LSP_PLUS_TEST_DBT=/path/to/dbt cargo test --test proxy -- --ignored
```

The last command runs the opt-in live tests against a real dbt 2, on a copy
of `tests/fixtures/shop` with a scratch home: the proxy in front of `dbt
lsp` with the proprietary build (tested with 2.0.9), alone with dbt-oss
(tested with 2.0.11).

The crate also builds `fake-dbt-lsp`, the scripted server of the tests; to
install the proxy alone, run `cargo install --path . --bin dbt-lsp-plus`.

The `Makefile` holds every check that CI runs, `make check` among them;
[the release pipeline](../reference/release-pipeline.md) describes how CI
runs them.
