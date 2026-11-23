# Build and test

```
nix develop
cargo build --release
cargo test
cargo test --locked -j 4 --test proxy --no-run
DBT_LSP_PLUS_TEST_DBT=/path/to/dbt unshare -rn cargo test --test proxy -- --ignored
```

The last two commands build the opt-in tests with the network, then run
them with none, as CI does: the corpus, and the live tests against a real
dbt 2, on a copy of `tests/fixtures/shop` with a scratch home, with either
build: dbt-oss (tested with 2.0.11), or the proprietary one (tested with
2.0.9), whose `dbt lsp` never runs.  Where there is no `unshare`, as on a
Mac, run the last one without it.

The crate also builds `fake-dbt-lsp`, the scripted server of the tests; to
install the proxy alone, run `cargo install --path . --bin dbt-lsp-plus`.

The `Makefile` holds every check that CI runs, `make check` among them;
[the release pipeline](../reference/release-pipeline.md) describes how CI
runs them.
