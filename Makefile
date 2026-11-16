# SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
#
# The checks of dbt-lsp-plus.  Run them from the repository's root in
# `nix develop`, which provides every tool but a C linker and `unshare`.
# Four workers by default, to keep a laptop cool: `make test CARGO_JOBS=8
# TEST_THREADS=8` widens them.

BASE ?= main
CARGO_JOBS ?= 4
TEST_THREADS ?= 4
CARGO_TEST := cargo test --locked -j $(CARGO_JOBS)
# The Python of the release, which has no extension in tests/bin.
PYTHON := scripts/release.py scripts/fetch-dbt tests/tools tests/bin/fake-gh
export PYTHONDONTWRITEBYTECODE := 1

.PHONY: check check-static fmt clippy public python test-python test conformance live commits

check: check-static test

check-static: fmt clippy public python test-python

fmt:
	@cargo fmt --check

clippy:
	@cargo clippy --locked -j $(CARGO_JOBS) --all-targets -- -D warnings

# Nothing that must not be published, and an SPDX line in each source file.
public:
	@scripts/check-public

python:
	@ruff check --no-cache $(PYTHON)

test-python:
	@python3 -m unittest discover -s tests/tools -p '*_test.py'

test:
	@$(CARGO_TEST) -- --test-threads=$(TEST_THREADS)

# Opt-in, with DBT_LSP_PLUS_TEST_DBT naming a dbt 2 executable: the corpus
# with either build, the live test with one that has `dbt lsp`.
conformance:
	@$(CARGO_TEST) --test proxy conformance:: -- --ignored --test-threads=$(TEST_THREADS)

live:
	@$(CARGO_TEST) --test proxy live:: -- --ignored --test-threads=$(TEST_THREADS)

# Each commit of BASE..HEAD: its title, its trailers, nothing private.
commits:
	@scripts/check-commits $(BASE)
