# Working in this repository

dbt-lsp-plus is a language server for dbt projects, from their files and
the user's `dbt parse`.  `README.md` describes it for users;
`docs/explanation/design.md` explains its design, what dbt was observed
to do, and what is left to do.

## Workflow

Every change follows the skill `workflow:plans`; load it before the first
edit.  In this repository: plans are `docs/work/plans/NNN-slug.md`, not
tracked; the checks are the `Makefile`'s, run in `nix develop`:

```
make check
```

and, before a merge, the opt-in tests with `DBT_LSP_PLUS_TEST_DBT` naming
dbt-oss, and again naming the proprietary build where it is at hand, as
CI's opt-in job runs it; built with the network, then run with none, as CI
runs them:

```
cargo test --locked -j 4 --test proxy --no-run
unshare -rn make live conformance DBT_LSP_PLUS_TEST_DBT=/path/to/dbt
```

Where there is no `unshare`, as on a Mac, the second runs unwrapped.

Each runs four workers at most (`CARGO_JOBS`, `TEST_THREADS`), and two
test runs in one checkout take turns.

CI and CD run on GitHub Actions once the repository is published
(`docs/reference/release-pipeline.md`).  `Cargo.toml`'s version, and the
package's entry in `Cargo.lock`, are written by the pipeline alone; each
release adds a commit to `origin/main`, so a clone pulls (`git pull
--ff-only`) before it merges.

## Layout

```
src/text.rs                   LSP positions, in UTF-16, and dbt's in code points; incremental changes
src/reference.rs              what a place of a document names: its Name
src/scan.rs                   one file of SQL and Jinja: blocks, CTEs, calls
src/yaml.rs                   YAML with the place of each scalar
src/properties.rs             the named entries of a YAML properties file
src/project.rs                the resources of a project and its packages; dbt's resource types, one table of what the server does with each, and how unique ids are written
src/cache.rs                  the scan kept between requests, each file by its metadata, an open one by its text's hash; the stamp of the files on disk
src/graph.rs                  the dependency graph: scanned, then the index's
src/sources.rs                references and renames of sources
src/navigation.rs             the Name at a position, from the document's scan, and where it leads: dbt's one lookup
src/features.rs               the answers of the added features, as LSP JSON
src/config.rs                 the user's Dbt, read once from the server command, and its parse: index, summary
src/run.rs                    the server's children: one run of dbt, the parse's or a command's, and the one wait for each child; dbt's first error and the parse's log
src/runs.rs                   the user's last run, from the files dbt left: its failures, the stale sources, where each is written; the model's Artefacts
src/capability.rs             what a session offers: one descriptor per method or command
src/commands.rs               the commands for clients, by dbt's CLI
src/packages.rs               whether dbt would install packages before it parses
src/locate.rs                 the project of a document, and whether a document lies in another; the CLI's resolution of a project, on the server's routes; the dbt of a project for server and CLI, the probe of a dbt
src/document.rs               folding, highlights, selection ranges, CTE rename
src/workspace.rs              the core as the front ends see it: the session's project, its dbt, the open texts, the scan kept and its first fill, the parse and its index, the graph, and when the index is stale; the node queries; what a request gets, found, empty, or unanswered with its reasons
src/proxy.rs                  the event loop of a session: its requests, notifications and diagnostics
src/cli.rs                    the command line and the subcommands doctor, select and node, from a shell; the project on the server's routes by its own resolution, its dbt by the server's
tests/proxy.rs                the binary with the fake dbt
tests/proxy/live.rs           the binary with the real dbt, either build, opt-in
tests/proxy/commands.rs       the commands for clients, by the fake dbt's CLI
tests/proxy/cli.rs            the subcommands doctor, select and node, the binary run as from a shell
tests/proxy/conformance.rs    the library's resolution against dbt's manifest, opt-in
tests/proxy/locate.rs         the walk up to a project and the lookup of its dbt, in scratch repositories
tests/proxy/routes.rs         the routes to the project: given, the client's folder, a document; and none
tests/proxy/plugin.rs         the Claude Code plugin's files: no path in them
tests/support/                the test client and fake-dbt, a scripted dbt
tests/fixtures/shop/          the fixture dbt project, on DuckDB
tests/fixtures/conformance/   the corpus's projects, one per rule, and its divergences.txt
Makefile                      the checks, which CI runs too
scripts/check-public          nothing private in the tracked files, an SPDX line in each source
scripts/check-commits         each commit's title, trailers and text, and no raise of a size ceiling unsaid
scripts/check-size            production lines and types of src/ against the Makefile's ceilings
scripts/release.py            CD's release: the version commit, the tag and its notes
scripts/fetch-dbt             CI's dbt by its pinned SHA-256, or the latest by its published one
scripts/bench/                synthetic projects and the server's times on them
tests/tools/                  the tests of the scripts; tests/bin/fake-gh stands in for gh
.github/workflows/            CI and CD, and the weekly runs on the latest dbt
docs/tutorials/               a first session, from the build to the call hierarchy
docs/how-to/                  configure an editor or an agent, build and test, troubleshoot
docs/reference/               the features by LSP method and their configuration, the subcommands, the release pipeline
docs/explanation/design.md    the design, what dbt was observed to do, what is left; the history of dbt lsp behind it
docs/social-media-preview.png GitHub's preview, from the untracked docs/work/social-preview/
.claude-plugin/               the marketplace of the repository, which names its one plugin
plugins/dbt-lsp-plus/         the Claude Code plugin: dbt-lsp-plus from PATH as the server of .sql, .yml and .yaml
```

## Rules of the code

- Route a method by its capability (`src/capability.rs`).
- The server's own requests to the client carry ids `dbt-lsp-plus/N`.
- What dbt does is observed, not documented: record each new observation
  in `docs/explanation/design.md`, make the fake dbt behave the same, and
  let the live test check it.
- No single-use private helper, but for these, each with its reason:
  - `Proxy::outline`, `Proxy::symbols`, `Proxy::hierarchy`,
    `Proxy::on_hover` and `Proxy::on_hints`: one function per method, so
    that `handle` stays a dispatch; `Proxy::handle`: the server's Answer
    by the handler of its method, which sends nothing, apart from
    `answer`, which asks for the index, contains a panic and sends the
    Answer or the failure;
  - `changes_graph`: the decision whether a client's notification may
    change the graph, free of the file system, with tests of its own;
  - `cache::unchanged`: the decision whether a project is built again,
    free of the file system, with tests of its own;
  - `commands::hooked`: the decision whether `dbt compile` would run a
    package's on-run hooks, the packages read where dbt installs them,
    with tests of its own, apart from `compile_job`'s words;
  - `project::versions_of`: the decision which versions a model's file
    is, free of the file system, with tests of its own, once every
    properties file of its package is read;
  - `project::resource_file`: which files the stamp of the disk counts,
    by the extensions of `LAYOUT` and YAML's, beside `LAYOUT` so that one
    table says both;
  - `Cache::build`: the reads of a refresh, through `&self`, so that the
    cache changes only once the project is built;
  - `Workspace::graph`: the graph kept while what it is made of stays,
    apart from the hierarchy's answer;
  - `Proxy::refresh_hints`: whether a client may be asked to refresh its
    hints, apart from the outcome of the parse that changes them;
  - `runs::locator`: where a failed node is written, apart from how it
    failed;
  - `Proxy::on_client`, `Proxy::on_request`, `Proxy::on_notification`,
    `Proxy::on_runs` and `Proxy::on_problems`: one handler per kind of
    message or event, so that `serve`, `on_client`, `on_request` and
    `on_parsed` stay dispatches; `Proxy::on_initialize`: the result of
    the crate's handshake, apart from `run`;
  - `capability::announced`: what a session announces, written out beside
    `CAPABILITIES`, its providers and commands held to the rows by a unit
    test, apart from `on_initialize`'s result;
  - `config::anchored`: the server command's relative paths made the
    server's directory's, apart from where dbt then runs, with tests of
    its own;
  - `Proxy::on_first_project`: the project that a document names, taken
    and started by `start_project`, apart from the notification that
    brings it, so that `on_notification` stays a sequence;
  - `Proxy::outside` and `Proxy::on_elsewhere`: a request on a document of
    another project, its reason and its answer, apart from `on_request`'s
    dispatch; `locate::elsewhere`: whether a document lies in another
    project, at the file system's boundary, with tests of its own;
  - `locate::repository`: the one run of git that finds a project's
    repository, at the boundary, its program and environment given;
    `locate::candidates`: the decision where a project's dbt may be, from
    git's answers, with tests of its own;
  - `Proxy::on_command`, `Proxy::run_command` and `Proxy::on_ran`: a
    request of a command for clients, its run by dbt's CLI, and the run's
    answer, one handler each, so that `on_request` and `serve` stay
    dispatches;
  - `commands::compiled` and `commands::listed`: the decisions on a run's
    output, with tests of their own;
    `commands::compile_job` and `commands::nodes_job`: one run per
    command, its words and its reader;
  - `cli::named`: the rule that a subcommand is the first word, its row,
    tested without running the binary, apart from `main.rs`, whose
    `cli::main` is the module's entry point; `cli::asked`: the decision on a
    subcommand's words, with tests of its own; `cli::doctor`,
    `cli::select` and `cli::node`, one per subcommand, each its row's run
    in `SUBCOMMANDS`; `cli::operand`, the decision on node's operand, a
    `Which`, with tests of its own; `cli::indexed`, the CLI's index, the kept manifest
    or a parse that it keeps, apart from the question `cli::node` asks;
  - `run::Running::spawn`: the start of a run's thread and its stop flag,
    which `Running::stop` ends, a resource's start;
  - `Proxy::on_source`, `Proxy::on_rename`, `Proxy::on_navigation` and
    `Proxy::on_document`: one answer per kind of request, as for the
    other methods, a definition and references sharing their reading of
    the position and the project;
  - `sources::replacement`, `sources::kept_relation`, `sources::refusal`
    and `sources::config_key`: each one rule of a rename (how a name is
    written where it goes; which lines keep the relation; when it is
    refused; where `dbt_project.yml` configures a source), with tests of
    its own;
  - `sources::argument_naming`: which argument of a call names a target;
  - `yaml::Tree::shares` and `yaml::Tree::repeats`: walks of the tree,
    which need a function of their own to recurse;
  - `Proxy::node_info`: the answer of the server's command;
  - `Proxy::on_parsed`, `Proxy::stop_parse` and `run::finish`: a parse's
    outcome and its end, each the end of a resource;
  - the sections of `tests/proxy/live.rs` (`document_features`,
    `navigation`, `last_run`, `lineage` and the others): one per observed
    behaviour, so that `with_the_real_dbt` stays their sequence; `found_from_a_document` and `found_from_the_folder`,
    one per route, and `from_a_shell`, the subcommands, so that
    `the_project_and_its_dbt_found_without_project_dir` stays theirs;
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
  - in `scripts/check-size`: `measure`, the count of one file, pure, with
    tests of its own, apart from `main`'s reading of `src/` and its
    verdict;
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
