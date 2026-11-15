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
DBT_LSP_PLUS_TEST_DBT=/path/to/dbt cargo test --test proxy -- --ignored
```

## Layout

```
src/rpc.rs                    framing; frames keep their bytes
src/text.rs                   LSP positions, encodings, incremental changes
src/scan.rs                   one file of SQL and Jinja: blocks, CTEs, calls
src/yaml.rs                   YAML with the place of each scalar
src/properties.rs             the named entries of a YAML properties file
src/project.rs                the resources of a project and its packages
src/graph.rs                  the dependency graph: scanned, then the server's
src/sources.rs                references and renames of sources
src/navigation.rs             the targets of definitions in YAML, dbt's lookup
src/features.rs               the answers of the added features, as LSP JSON
src/config.rs                 the user's dbt parse: its command, run, index, summary
src/document.rs               folding, highlights, selection ranges, CTE rename
src/proxy.rs                  the event loop, the server process, the listing
tests/proxy.rs                the binary in front of the fake server
tests/proxy/live.rs           the binary in front of the real dbt lsp, opt-in
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
- No single-use private helper, but for these, each with its reason:
  - `Proxy::outline` and `Proxy::hierarchy`: one function per owned method,
    so that `answer` stays a dispatch;
  - `Proxy::expire_listing`, `Proxy::start_restart` and
    `Proxy::stop_server`: the deadline of a listing, and the start and the
    teardown of the server process, each a resource's start or end;
  - `restart_delay`: the decision on a lost server, free of the clock,
    with tests of its own;
  - `project_dir`: the parsing of the server command, at the boundary;
  - `graph::resolve`: dbt's lookup rule, in one place;
  - `Pending::merged`: what each kind of forwarded request does to the
    server's answer, so that `on_server` stays a dispatch;
  - `Proxy::refresh_hints`: the proxy's one request to the client, its id
    and the answer it keeps;
  - `runs::place`: where a failed node is written, apart from how it
    failed;
  - `Proxy::on_server_response`, `Proxy::on_server_diagnostics`,
    `Proxy::on_registration`, `Proxy::on_progress`, `Proxy::on_runs`,
    `Proxy::on_restarted` and `Proxy::on_time`: one handler per kind of
    message or event, so that `on_server`, `on_own_response`, `serve` and
    `handle` stay dispatches;
  - `answered_as_2_0_9` in `tests/support/fake_dbt_lsp.rs`: what dbt lsp
    2.0.9 answers, apart from what the script makes it answer;
  - `Fake::misbehaves` in `tests/support/fake_dbt_lsp.rs`: what the
    script makes one life do in place of an answer, apart from `main`'s
    loop over frames;
  - `Proxy::local_method`, `Proxy::targeted`, `Proxy::on_source`,
    `Proxy::on_rename`, `Proxy::on_definition` and `Proxy::on_document`: the
    routing of a request the proxy may answer itself (by its method, then
    by what its position holds), and one answer per kind of request, as
    for the owned methods;
  - `sources::replacement`, `sources::kept_relation`, `sources::refusal`
    and `sources::config_key`: each one rule of a rename (how a name is
    written where it goes; which lines keep the relation; when it is
    refused; where `dbt_project.yml` configures a source), with tests of
    its own;
  - `sources::argument_naming`: which argument of a call names a target;
  - `yaml::Node::shares`: a walk of the tree, which needs a function of its
    own to recurse;
  - `Proxy::node_info`, `Proxy::remember` and `Proxy::handle`: the answer of
    the proxy's command; what the proxy keeps of a forwarded request apart
    from where it goes; one event's dispatch apart from the waiting with
    the listing's deadline;
  - `Proxy::on_parsed`, `Proxy::stop_parse` and `config::finish`: a parse's
    outcome and its end, each the end of a resource, as `stop_server`.
