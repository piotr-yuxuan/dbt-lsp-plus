# SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
#
# The checks of dbt-lsp-plus.  Run them from the repository's root in
# `nix develop`, which provides every tool but a C linker and `unshare`.
# Four workers by default, to keep a laptop cool: `make test CARGO_JOBS=8
# TEST_THREADS=8` widens them.

CARGO_JOBS ?= 4
TEST_THREADS ?= 4
CARGO_TEST := cargo test --locked -j $(CARGO_JOBS)

.PHONY: check check-static fmt clippy test conformance live

check: check-static test

check-static: fmt clippy

fmt:
	@cargo fmt --check

clippy:
	@cargo clippy --locked -j $(CARGO_JOBS) --all-targets -- -D warnings

test:
	@$(CARGO_TEST) -- --test-threads=$(TEST_THREADS)

# Opt-in, with DBT_LSP_PLUS_TEST_DBT naming a dbt 2 executable: the corpus
# with either build, the live test with one that has `dbt lsp`.
conformance:
	@$(CARGO_TEST) --test proxy conformance:: -- --ignored --test-threads=$(TEST_THREADS)

live:
	@$(CARGO_TEST) --test proxy live:: -- --ignored --test-threads=$(TEST_THREADS)
