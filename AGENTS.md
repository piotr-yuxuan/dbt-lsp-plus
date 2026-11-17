# Working in this repository

dbt-lsp-plus is a proxy in front of `dbt lsp` that adds LSP features the
server lacks.  `README.md` describes it for users;
`docs/explanation/design.md` explains its design, what `dbt lsp` was
observed to do, and what is left to do.

## Workflow

Every change follows the skill `workflow:plans`; load it before the first
edit.  In this repository: plans are `docs/work/plans/NNN-slug.md`, not
tracked; the checks are the `Makefile`'s, run in `nix develop`:

```
make check
```

and, before a merge, the opt-in tests with `DBT_LSP_PLUS_TEST_DBT` naming a
dbt 2 executable, once with the proprietary build, which has `dbt lsp`, and
once with dbt-oss, which has none and runs the proxy alone:

```
make live conformance DBT_LSP_PLUS_TEST_DBT=/path/to/dbt
```

Each runs four workers at most (`CARGO_JOBS`, `TEST_THREADS`), and two
test runs in one checkout take turns.

CI and CD run on GitHub Actions once the repository is published
(`docs/reference/release-pipeline.md`).  `Cargo.toml`'s version, and the
package's entry in `Cargo.lock`, are written by the pipeline alone; each
release adds a commit to `origin/main`, so a clone pulls (`git pull
--ff-only`) before it merges.

## Layout

```
src/rpc.rs                    framing; frames keep their bytes
src/text.rs                   LSP positions, encodings, incremental changes
src/scan.rs                   one file of SQL and Jinja: blocks, CTEs, calls
src/yaml.rs                   YAML with the place of each scalar
src/properties.rs             the named entries of a YAML properties file
src/project.rs                the resources of a project and its packages
src/cache.rs                  the scan kept between requests, each file by its metadata
src/graph.rs                  the dependency graph: scanned, then the server's or the manifest's
src/sources.rs                references and renames of sources
src/navigation.rs             the targets of definitions and references, dbt's one lookup
src/features.rs               the answers of the added features, as LSP JSON
src/config.rs                 the user's dbt parse: its command, run, index, summary
src/packages.rs               whether dbt would install packages before it parses
src/parse_log.rs              the errors and warnings of dbt parse's JSON log
src/document.rs               folding, highlights, selection ranges, CTE rename
src/proxy.rs                  the event loop, the server process, the listing
tests/proxy.rs                the binary in front of the fake server
tests/proxy/live.rs           the binary in front of the real dbt lsp, or alone with dbt-oss, opt-in
tests/proxy/conformance.rs    the library's resolution against dbt's manifest, opt-in
tests/support/                the test client and fake-dbt-lsp
tests/fixtures/shop/          the fixture dbt project, on DuckDB
tests/fixtures/conformance/   the corpus's projects, one per rule, and its divergences.txt
Makefile                      the checks, which CI runs too
scripts/check-public          nothing private in the tracked files, an SPDX line in each source
scripts/check-commits         each commit's title, trailers and text
scripts/release.py            CD's release: the version commit, the tag and its notes
scripts/fetch-dbt             CI's dbt by its pinned SHA-256, or the latest by its published one
scripts/bench/                synthetic projects and the proxy's times on them
tests/tools/                  the tests of the scripts; tests/bin/fake-gh stands in for gh
.github/workflows/            CI and CD, and the weekly runs on the latest dbt
docs/tutorials/               a first session, from the build to the call hierarchy
docs/how-to/                  configure an editor or an agent, use it with dbt-oss alone, build and test, troubleshoot
docs/reference/               the features by LSP method and their configuration, the release pipeline
docs/explanation/design.md    the design, what dbt lsp was observed to do, what is left
docs/diagrams/                diagram sources, light and dark, in diagram-design's sanityinc-tomorrow profile
docs/images/                  their PNG exports, by headless Chromium at twice the size
docs/social-media-preview.png GitHub's preview, from the untracked docs/work/social-preview/
```

## Rules of the code

- Forward what the proxy does not own byte for byte; never re-serialise it.
- Own a method only when the server does not announce it.
- The proxy's own requests carry ids `dbt-lsp-plus/N`, and their answers
  never reach the client.
- What `dbt lsp` does is observed, not documented: record each new
  observation in `docs/explanation/design.md`, make the fake server
  behave the same, and let the live test check it.
- No single-use private helper, but for these, each with its reason:
  - `Proxy::outline` and `Proxy::hierarchy`: one function per owned method,
    so that `answer` stays a dispatch;
  - `Proxy::expire_listing`, `Proxy::start_restart` and
    `Proxy::stop_server`: the deadline of a listing, and the start and the
    teardown of the server process, each a resource's start or end;
  - `restart_delay`: the decision on a lost server, free of the clock,
    with tests of its own;
  - `changes_graph`: the decision whether a client's notification may
    change the graph, free of the file system, with tests of its own;
  - `cache::unchanged`: the decision whether a project is built again,
    free of the file system, with tests of its own;
  - `Cache::build`: the reads of a refresh, through `&self`, so that the
    cache changes only once the project is built;
  - `Proxy::graph`: the graph kept while what it is made of stays, apart
    from the hierarchy's answer;
  - `project_dir`: the parsing of the server command, at the boundary;
  - `Pending::merged`: what each kind of forwarded request does to the
    server's answer, so that `on_server` stays a dispatch;
  - `Proxy::refresh_hints`: whether a client may be asked to refresh its
    hints, apart from the outcome of the parse that changes them;
  - `runs::place`: where a failed node is written, apart from how it
    failed;
  - `Proxy::on_server_response`, `Proxy::on_server_diagnostics`,
    `Proxy::on_registration`, `Proxy::on_progress`, `Proxy::on_runs`,
    `Proxy::on_restarted`, `Proxy::on_time`, `Proxy::on_notification` and
    `Proxy::on_problems`: one handler per kind of message or event, so that
    `on_server`, `on_own_response`, `serve`, `handle`, `on_client` and
    `on_parsed` stay dispatches;
  - `lacks_lsp`: the probe of the user's dbt, at the boundary, with tests
    of its own;
  - `packages::install_path`, `packages::read` and `packages::named`: one
    rule each of how dbt finds a project's packages (where it installs
    them, which files it reads, which local packages it follows), apart
    from what `packages::refused` decides;
  - `answered_as_2_0_9` in `tests/support/fake_dbt_lsp.rs`: what dbt lsp
    2.0.9 answers, apart from what the script makes it answer;
  - `Fake::misbehaves` in `tests/support/fake_dbt_lsp.rs`: what the
    script makes one life do in place of an answer, apart from `main`'s
    loop over frames;
  - `Proxy::local_method`, `Proxy::targeted`, `Proxy::on_source`,
    `Proxy::on_rename`, `Proxy::on_definition`, `Proxy::on_references` and
    `Proxy::on_document`: the
    routing of a request the proxy may answer itself (by its method, then
    by what its position holds), and one answer per kind of request, as
    for the owned methods;
  - `sources::replacement`, `sources::kept_relation`, `sources::refusal`
    and `sources::config_key`: each one rule of a rename (how a name is
    written where it goes; which lines keep the relation; when it is
    refused; where `dbt_project.yml` configures a source), with tests of
    its own;
  - `sources::argument_naming`: which argument of a call names a target;
  - `yaml::Node::shares` and `yaml::Node::repeats`: walks of the tree,
    which need a function of their own to recurse;
  - `Proxy::node_info`, `Proxy::remember` and `Proxy::handle`: the answer of
    the proxy's command; what the proxy keeps of a forwarded request apart
    from where it goes; one event's dispatch apart from the waiting with
    the listing's deadline;
  - `Proxy::on_parsed`, `Proxy::stop_parse` and `config::finish`: a parse's
    outcome and its end, each the end of a resource, as `stop_server`;
  - the sections of `tests/proxy/live.rs` (`document_features`,
    `navigation`, `last_run`, `lineage`, `watcher` and the others): one
    per observed behaviour, so that `in_front_of_the_real_dbt_lsp` stays
    their sequence;
  - `parse` in `tests/proxy/conformance.rs`: dbt's run, the corpus's one
    effect, apart from the comparisons, which decide;
  - `nodes_compared`, `calls_compared` and `entries_compared` in
    `tests/proxy/conformance.rs`: one per kind of answer compared with
    dbt's, so that `corpus` stays a walk over the cases; `references_missed`
    there, the check of definitions against references, which no registry
    excuses;
  - in `scripts/release.py`, ported from imprimeur: `next_version`,
    `current_version`, `release_notes`, `with_version` and `decide`, the
    pure decisions, each with tests of its own, apart from the effects;
    `read`, `has_release` and `confirm_ci`, one per question asked of git
    or GitHub, so that `main` stays the run's order;
  - `fetch` in `scripts/fetch-dbt`: the download and its checks, with tests
    of their own, apart from `main`, which reads the arguments and turns
    failures into status 2; `latest_oss`, `latest_proprietary` and
    `sidecar_digest`, the pure decisions on what the latest build is and
    its digest, with tests on recorded listings; `find_oss` and
    `find_proprietary`, one per build, behind `LATEST`;
  - in `scripts/bench/generate.py`: `model_sql`, `column_yaml` and
    `sources_yaml`, the text of one kind of file; `fixed_files`, `models`,
    `others` and `big_yaml`, one part of the project each, so that `main`
    stays the list of the parts; in `scripts/bench/measure.py`: `first`,
    the session of a first request, apart from the warm one; `saved`, the
    files a run saves, beside `requests`, the requests it times;
  - `commit_on_branch`, `create_release`, `graphql` and `runs_of_ci` in
    `tests/bin/fake-gh`: one per call of GitHub's, so that `main` stays a
    dispatch.
