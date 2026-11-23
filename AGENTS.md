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
src/node.rs                   dbt's resource types, one table of what the proxy does with each, and how unique ids are written
src/reference.rs              what a place of a document names: its Name, and the flag's word for its kind
src/scan.rs                   one file of SQL and Jinja: blocks, CTEs, calls
src/yaml.rs                   YAML with the place of each scalar
src/properties.rs             the named entries of a YAML properties file
src/project.rs                the resources of a project and its packages
src/cache.rs                  the scan kept between requests, each file by its metadata; the stamp of the files on disk
src/graph.rs                  the dependency graph: scanned, then the server's or the manifest's
src/sources.rs                references and renames of sources
src/navigation.rs             the Name at a position, from the document's scan, and where it leads: dbt's one lookup
src/features.rs               the answers of the added features, as LSP JSON
src/answer.rs                 what a request gets: found, empty, or unanswered with its reasons, as LSP JSON
src/config.rs                 the user's Dbt, read once from the server command, and its parse: index, summary
src/run.rs                    the proxy's children but dbt lsp: one run of dbt, the parse's or a command's, and the one wait for each child
src/runs.rs                   the user's last run, from the files dbt left: its failures, the stale sources, where each is written; the model's Artefacts
src/capability.rs             what a session offers: one descriptor per method or command
src/modes.rs                  who answers each method: the user's settings and the session's table
src/commands.rs               the commands for clients: by dbt lsp's own, or by dbt's CLI under the cut
src/shadow.rs                 the proxy's definitions and references against dbt lsp's: compared, counted, logged
src/packages.rs               whether dbt would install packages before it parses
src/locate.rs                 the project of a document, and whether a document lies in another; the dbt of a project, the probe of a dbt
src/parse_log.rs              the errors and warnings of dbt parse's JSON log
src/document.rs               folding, highlights, selection ranges, CTE rename
src/workspace.rs              the core as the front ends see it: the session's project, its dbt, the scan kept and its first fill, the parse and its index, the graph, and when the index is stale; the node queries
src/proxy.rs                  the event loop; the Backend, dbt lsp's process: its life, its listings
src/cli.rs                    the subcommands doctor, select and node, from a shell, on the server's resolution
tests/proxy.rs                the binary in front of the fake server
tests/proxy/live.rs           the binary in front of the real dbt lsp, or alone with dbt-oss, opt-in
tests/proxy/shadow.rs         the shadow in front of the fake server
tests/proxy/commands.rs       the commands for clients, in front of the fake and alone
tests/proxy/cli.rs            the subcommands doctor, select and node, the binary run as from a shell
tests/proxy/conformance.rs    the library's resolution against dbt's manifest, opt-in
tests/proxy/locate.rs         the walk up to a project and the lookup of its dbt, in scratch repositories
tests/proxy/routes.rs         the routes to the project: given, the client's folder, a document; and none
tests/proxy/plugin.rs         the Claude Code plugin's files: no path in them
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
docs/reference/               the features by LSP method and their configuration, the subcommands, the release pipeline
docs/explanation/design.md    the design, what dbt lsp was observed to do, what is left
docs/diagrams/                diagram sources, light and dark, in diagram-design's sanityinc-tomorrow profile
docs/images/                  their PNG exports, by headless Chromium at twice the size
docs/social-media-preview.png GitHub's preview, from the untracked docs/work/social-preview/
.claude-plugin/               the marketplace of the repository, which names its one plugin
plugins/dbt-lsp-plus/         the Claude Code plugin: dbt-lsp-plus from PATH as the server of .sql, .yml and .yaml
```

## Rules of the code

- Forward what the proxy does not own or merge byte for byte; never
  re-serialise it.
- Route a method by its capability (`src/capability.rs`) and its mode
  (`src/modes.rs`); unset, a mode is its shipped default: the methods
  the proxy adds its own only when the server does not announce them,
  the others `merge`.
- The proxy's own requests carry ids `dbt-lsp-plus/N`, and their answers
  never reach the client.
- What `dbt lsp` does is observed, not documented: record each new
  observation in `docs/explanation/design.md`, make the fake server
  behave the same, and let the live test check it.
- No single-use private helper, but for these, each with its reason:
  - `Proxy::outline`, `Proxy::symbols` and `Proxy::hierarchy`: one
    function per owned method, so that `plus_answer` stays a dispatch;
    `Proxy::plus_answer`: the proxy's Answer by the handler of its
    method, which sends nothing, apart from `answer`, which asks for the
    index first, contains a panic and sends the Answer or the failure;
  - `Proxy::expire_listing`, `Proxy::tell_stalled`, `Proxy::start_restart`
    and `Proxy::stop_server`: the deadline of a listing and that of a
    compilation's end, and the start and the teardown of the server
    process, each a resource's start or end;
  - `restart_delay`: the decision on a lost server, free of the clock,
    with tests of its own;
  - `changes_graph`: the decision whether a client's notification may
    change the graph, free of the file system, with tests of its own;
  - `compile_failure`: what a compilation that compiled nothing reports,
    read at the boundary, free of the session, with tests of its own;
  - `cache::unchanged`: the decision whether a project is built again,
    free of the file system, with tests of its own;
  - `project::versions_of`: the decision which versions a model's file
    is, free of the file system, with tests of its own, once every
    properties file of its package is read;
  - `project::resource_file`: which files the stamp of the disk counts,
    by the extensions of `LAYOUT` and YAML's, beside `LAYOUT` so that one
    table says both;
  - `Cache::build`: the reads of a refresh, through `&self`, so that the
    cache changes only once the project is built;
  - `Workspace::graph`: the graph kept while what it is made of stays,
    apart from the hierarchy's answer; `Workspace::notice`: a look at the
    disk on behalf of a request, the core's, public for the front ends,
    though the LSP front end alone calls it yet (MCP in plan 052);
  - `Pending::merged` and `Pending::found`: what each kind of part does
    to the server's answer, and what a part found is alone, so that
    `on_server` and `unserved` stay dispatches;
  - `Proxy::refresh_hints`: whether a client may be asked to refresh its
    hints, apart from the outcome of the parse that changes them;
  - `runs::locator`: where a failed node is written, apart from how it
    failed;
  - `Proxy::on_server_response`, `Proxy::on_server_diagnostics`,
    `Proxy::on_registration`, `Proxy::on_progress`, `Proxy::on_runs`,
    `Proxy::on_restarted`, `Proxy::on_time`, `Proxy::on_notification`,
    `Proxy::on_problems` and `Proxy::on_compiled`: one handler per kind of
    message or event, so that `on_server`, `on_own_response`, `serve`,
    `handle`, `on_client` and `on_parsed` stay dispatches;
  - `config::anchored`: the server command's relative paths made the
    proxy's directory's, apart from where dbt then runs, with tests of its
    own;
  - `Proxy::on_first_project`: the start of a project that a document
    names, apart from the notification that brings it, so that
    `on_notification` stays a sequence;
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
    answer, one handler each, so that `on_request` and `handle` stay
    dispatches;
  - `commands::route`, `commands::announced`, `commands::arguments`,
    `commands::compiled` and `commands::listed`: the decisions on a
    command's request and on a run's output, with tests of their own;
    `commands::compile_job` and `commands::nodes_job`: one run per
    command, its words and its reader;
  - `cli::named`: the rule that a subcommand is the first word, tested
    without running the binary, apart from `main.rs`, whose `cli::main` is
    the module's entry point; `cli::asked`: the decision on a
    subcommand's words, with tests of its own; `cli::doctor`,
    `cli::select` and `cli::node`, one per subcommand, each its row's run
    in `SUBCOMMANDS`; `cli::operand`, the decision on node's operand, with
    tests of its own; `cli::indexed`, the CLI's index, the kept manifest
    or a parse that it keeps, apart from the question `cli::node` asks;
  - `Proxy::compare`: the shadow's work on a followed request's answer,
    once the client has it, so that `on_server_response` stays a dispatch;
  - `shadow::read` and `shadow::compare`: the decisions on dbt lsp's
    answer and on how two answers stand, free of the file system, with
    tests of their own;
  - `shadow::target`, `shadow::oracle`, `shadow::start`, `Record::line`
    and `Counts::end`: the log's lines in one place, apart from where the
    proxy reads what they hold;
  - `Log::open`, `Log::append` and `Shadow::write`: the start of the
    shadow's log and its writes, a resource's start and use;
  - `run::Running::spawn`: the start of a run's thread and its stop flag,
    which `Running::stop` ends, a resource's start, as `Log::open` and
    `Proxy::start_restart` are, where `Proxy::stop_server` is an end;
  - `run::Output::error_line`: the list commands' reader of dbt's first
    error, a text log's `[error]` line, kept apart from `error_event` so
    that plan 048 changed no answer; plan 071, which reads dbt's output
    apart from its log, merges the two;
  - `packages::install_path`, `packages::read` and `packages::named`: one
    rule each of how dbt finds a project's packages (where it installs
    them, which files it reads, which local packages it follows), apart
    from what `packages::refused` decides;
  - `answered_as_2_0_9` in `tests/support/fake_dbt_lsp.rs`: what dbt lsp
    2.0.9 answers, apart from what the script makes it answer;
  - `Fake::misbehaves` in `tests/support/fake_dbt_lsp.rs`: what the
    script makes one life do in place of an answer, apart from `main`'s
    loop over frames;
  - `Fake::compiles_as_2_0_9` in `tests/support/fake_dbt_lsp.rs`: which
    of the client's notifications make dbt lsp 2.0.9 compile, and what
    they leave for the next (whether a document opened, the last
    `didSave` and when, for the save window), as 2.0.9 does, apart from
    `Fake::notified`'s dispatch of the script's own notifications, which
    with these arms would pass a complexity of 10;
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
  - `yaml::Tree::shares` and `yaml::Tree::repeats`: walks of the tree,
    which need a function of their own to recurse;
  - `Proxy::node_info`, `Proxy::remember` and `Proxy::handle`: the answer of
    the proxy's command; what the proxy keeps of a forwarded request apart
    from where it goes; one event's dispatch apart from the waiting with
    the listing's deadline;
  - `Proxy::hover_part`: one concern, what a hover's answer gets first (a
    node's summary and facts, a docs block, a test's signature) or why it
    has nothing, apart from what the proxy keeps of other requests, as
    `on_definition` and the others are for `plus_answer`;
  - `Proxy::union_part`: one concern, what a definition or references
    gets first when the proxy unites dbt lsp's answer with its own, or
    nothing, apart from what the proxy keeps of other requests, as
    `hover_part` is for a hover;
  - `Proxy::on_parsed`, `Proxy::stop_parse` and `run::finish`: a parse's
    outcome and its end, each the end of a resource, as `stop_server`;
  - the sections of `tests/proxy/live.rs` (`document_features`,
    `navigation`, `last_run`, `lineage`, `watcher` and the others): one
    per observed behaviour, so that `in_front_of_the_real_dbt_lsp` stays
    their sequence; `found_from_a_document` and `found_from_the_folder`,
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
