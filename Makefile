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
# The Python of the scripts and of their tests, some without an extension.
PYTHON := scripts/release.py scripts/fetch-dbt scripts/check-size scripts/bench tests/tools tests/bin/fake-gh
export PYTHONDONTWRITEBYTECODE := 1
# The ceilings of src/'s production lines and types, which each plan of the
# reduction chain lowers to its own figures.  A commit that raises one, or
# changes how they are checked, says why in a line of its body:
# "Raises the size ceilings: WHY".
SIZE_LINES := 6501
SIZE_TYPES := 97

.PHONY: check check-static fmt clippy public python test-python workflows size test conformance live commits

check: check-static test

check-static: fmt clippy public python test-python workflows size

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

workflows:
	@actionlint

size:
	@scripts/check-size $(SIZE_LINES) $(SIZE_TYPES)

test:
	@$(CARGO_TEST) -- --test-threads=$(TEST_THREADS)

# Opt-in, with DBT_LSP_PLUS_TEST_DBT naming a dbt 2 executable: the corpus
# and the live test with either build, the proxy in front of `dbt lsp` with
# the proprietary one, alone with dbt-oss.
conformance:
	@$(CARGO_TEST) --test proxy conformance:: -- --ignored --test-threads=$(TEST_THREADS)

live:
	@$(CARGO_TEST) --test proxy live:: -- --ignored --test-threads=$(TEST_THREADS)

# Each commit of BASE..HEAD: its title, its trailers, nothing private, no
# raise of a size ceiling, nor change to its check, without
# "Raises the size ceilings:" in its body.
commits:
	@scripts/check-commits $(BASE)
