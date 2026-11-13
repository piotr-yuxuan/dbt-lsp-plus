# Design of dbt-lsp-plus

dbt-lsp-plus sits between any LSP client and the proprietary `dbt lsp` of
dbt 2 (observed on 2.0.9).  It forwards every message unchanged and answers
itself the requests of the features that the server does not announce.  An
editor gets them through standard LSP, and so does an agent whose LSP tool
speaks definition, references, hover, document and workspace symbols and the
call hierarchy.

## Boundary

- It uses only the server's interface (its messages, its announced
  capabilities and its custom command `dbt.listNodes`) and the user's own
  dbt CLI, whose `dbt parse` gives the resolved configuration that the
  server does not expose.
- It unlocks nothing that dbt gates behind an account.  Column-level
  features stay as the server gives them.
- It runs the `dbt` the user installed, and ships no part of it.
- Standard LSP is its only contract with an editor.  dbt-mode needs nothing
  specific to it: dbt-mode steps aside, feature by feature, for each
  capability the server announces, and keeps working without the proxy.

## Language: Rust

- One static binary that any editor or agent starts as a stdio server, with
  no runtime to install beside dbt.
- refmux (`circonomit/refmux`, Apache-2.0), the prior art, is Rust.  It runs
  one `dbt lsp` per project of a monorepo and adds no features.  The two
  compose as two processes, and code can move between them later.
- Python with pygls was set aside: pygls is a server framework that answers
  for itself, while a proxy must pass unknown methods, `dbt/*`
  notifications and the server's own requests byte for byte.  Go would serve
  as well as Rust; Rust won on refmux.

## Message flow

```
client ── stdin ──▶ dbt-lsp-plus ── stdin ──▶ dbt lsp
       ◀─ stdout ──              ◀─ stdout ──
```

One thread reads each input and sends whole frames to one event loop, which
owns all state and writes both outputs.

- Client to server: every frame is forwarded as it arrived, its bytes
  untouched, a body that is not JSON included.  The exceptions are the
  requests of an owned method, which the proxy answers itself, and the
  client's answers to the proxy's own requests (`workspace/inlayHint/refresh`).
- Server to client: every frame is forwarded as it arrived, except:
  - the answer to `initialize`, to which the proxy adds the capabilities
    the server lacks (`documentSymbolProvider`, `workspaceSymbolProvider`,
    `callHierarchyProvider`), and whose `positionEncoding` it adopts
    (UTF-16 when absent, as with 2.0.9), and its `experimental.dbtLspPlus`
    and its command `dbt-lsp-plus.nodeInfo`;
  - the answer to a hover on a `ref` or a `source`, which gets a summary
    first (see "Node information and hover"), and to an inlay hint
    request, which gets the proxy's hints after the server's (see "Inlay
    hints");
  - the server's diagnostics of a file where the user's last run failed,
    which get the proxy's appended (see "The last run as diagnostics");
  - the answers to the proxy's own requests, which never reach the client.
- References and renames stay the server's, but on a source the server
  answers them with nothing: the proxy answers those requests itself, when
  the position is on a `source()` argument or on a source's or table's
  `name:` under `sources:`, in a file the scan holds, and forwards every
  other one.  Definitions likewise, where the document shows a target that
  the server answers with null (see "Navigation from YAML").  It says so
  under the standard `capabilities.experimental`, merged with the
  server's: `"dbtLspPlus": {"version": 1, "references": ["source"],
  "rename": ["source", "cte"], "definition": ["source", "ref", "doc",
  "entry", "test"]}` names, per request, the kinds of target it answers
  itself, and dbt-mode steps aside for exactly those.
- A method is owned only when the server does not announce it.  dbt lsp
  2.0.9 announces `documentSymbolProvider: false` but answers that request
  with a flat list of tokens, so the proxy owns it.  A later server that
  announces a feature gets its requests back.
- Request ids: the client's ids and the server's ids pass unchanged.  The
  proxy's own requests carry string ids `dbt-lsp-plus/N`, never one that a
  pending request of the client's carries; an answer is the proxy's own only
  when its id has that prefix and answers no request of the client's.  A
  request of the client's under the id of the proxy's listing in flight is
  refused with -32600, since the server would get two requests under one id.
- A panic while the proxy answers a request fails that request with -32603;
  the session goes on.  No input is known to cause one, so no test does.

## Root

The project directory is the server command's `--project-dir` (`--project-dir
X` or `--project-dir=X`), so that the proxy reads the project the server
runs on; else the client's first workspace folder or `rootUri`, when it holds
a `dbt_project.yml`; else the working directory.

The URIs the proxy returns are built on the root, so the root keeps the
spelling the user or the client gave it:
- `--project-dir` is made absolute as spelt, links unresolved, each `..`
  dropping the component before it; it is resolved only when that would name
  another directory, through a link before a `..`;
- when the client's folder is the same directory under another spelling (a
  symbolic link), the client's spelling wins.

A document under the root as spelt keeps its path, links within the project
included, since the scan walks them so.  A document reached through another
spelling of its directory is mapped onto the root by that directory's
resolved path.

## State

- Open documents: the text of each, kept from `didOpen` and the incremental
  `didChange` (UTF-16, UTF-8 or UTF-32 units), so that the outline follows
  unsaved edits.
- The project: read on each request from `dbt_project.yml`, its resource
  paths, through symbolic links, and the packages under
  `packages-install-path`, with the open texts winning over the files on
  disk.  Seeds are found but not read.  This finds:
  - models, seeds, snapshots, analyses and singular tests;
  - macros, generic tests and docs blocks;
  - source tables, from the YAML of every resource path, parsed with the
    place of each scalar as written and where each mapping ends;
  - the `ref`, `source`, `adapter.dispatch` and macro calls inside the Jinja
    tags of each body.  A tag inside a SQL comment counts, since Jinja
    renders it; a `ref(` in prose does not.
- The scan of SQL and Jinja is lexical, not a parser:
  - Jinja tags end at their first closer outside strings and nested braces,
    and the content of a raw block holds none;
  - CTEs are found after `with` or a comma, outside Jinja, SQL comments and
    strings, so a CTE that Jinja generates is not one but those after it
    are;
  - names may hold any letter.
  There is no cache yet.  A workspace symbol or call hierarchy request
  rescans everything, about 40 ms on 2,000 synthetic models of 40 lines
  each, in a release build.
- The graph: once the server has listed the graph, the dependencies of each
  node it listed are its own, since only dbt resolves `ref(var('x'))`; until
  then, and for the nodes it did not list, they come from the scan.  The
  server lists no macros, so the edges to and from macros always come from
  the scan.  The scan looks a ref or a source up in the caller's package,
  the root project, then any package, as dbt 2.0.9 does by default, and
  an unqualified macro in the caller's package and the root project only, as
  dbt does: another package's macros need its name.  A dispatch names every
  `*__name` implementation of its package.  A listing that fails keeps the
  graph it had.  An empty `*` listing counts as a failure, since the scan
  found resources; an empty `source:*` does not.  After a crash, the scan
  gives the whole graph again.
- A source that `overrides` a package's source is that package's source:
  dbt patches it, so the proxy places its tables in the package.
- Text read from disk loses its byte order mark, as an editor's document
  does, so that positions on its first line match.

### What `dbt.listNodes` does, observed on 2.0.9

- It reads only its first argument.  `*` lists models, seeds, snapshots,
  analyses and tests, but no sources, which need `source:*`.  No selector
  lists macros, and `depends_on` names nodes only.
- A second `dbt.listNodes` in flight cancels the first: the first is
  answered `{"error": "Operation cancelled", "nodes": []}`.
- Each one makes the server compile, and `dbt/lspCompileComplete` follows
  its answer.  The first prototype listed on that notification, which made
  a loop: 565 compilations in two minutes.
- The server compiles nothing after `initialized` until a document opens or
  a request comes.

So the proxy lists `*` and then `source:*`, one after the other.  It lists
on the client's signals only:
- the first `didOpen`, and no later one, even when that listing failed:
  each listing compiles the whole project, and an editor that restores a
  session opens many documents; the next change lists again;
- `didSave`;
- `workspace/didChangeWatchedFiles`, and files created, renamed or deleted.

The server gets the client's change before the listing it prompts.  The
proxy never lists while a `dbt.listNodes` of the client's is pending, nor
sends the second selector then, nor anything after `shutdown`.  It retries a
cancelled listing at most three times, and abandons one unanswered after two
minutes, on time, even when the client is quiet; no test waits the two
minutes, so only a unit test of the abandonment covers it.

## Sources

- References of a source table, or of a whole source: every `source()`
  call that names it, in SQL inside Jinja tags, in YAML without them (a
  relationships test's `to:`), in each package's `dbt_project.yml` (vars,
  hooks) and in a Python model (`dbt.source()`); the key that configures it
  under `sources:` in the root `dbt_project.yml`; and its declarations when
  the client asks.  A call whose argument is an expression, as in
  `source('raw', 'events' ~ var('suffix'))`, names no table.
- The proxy answers only on a file the scan holds: on a file not saved yet,
  or one outside the resource paths, it could not see every occurrence, so
  the request goes to the server.
- An open document is named by the URI the client opened it under, and its
  ranges are those of its open text.
- A rename keeps the relation in the warehouse.  dbt takes a table's
  identifier from its name, and a source's schema from its name, unless they
  are set: `dbt parse` gives `events.page_views`, declared without `schema:`,
  the schema `events`.  So renaming a table without `identifier:` inserts
  `identifier: <old>` after its `name:` line, at the key's indentation and
  with the line's own line break, and renaming a source without `schema:`
  inserts `schema: <old>`.  The live test renames `events.page_views` and
  checks that dbt still compiles the model that selects from it to
  `page_views`.
- A name inside quotes is replaced inside them, and must not hold that quote
  or a backslash.  An unquoted YAML value is written plain when it is a plain
  identifier that YAML reads as a string, else double-quoted: `null`,
  `true`, `false`, `yes`, `no`, `on`, `off`, `y` and `n`, in any case, are
  quoted.
- A rename is refused, never applied in part, when:
  - the source is declared nowhere, or in several packages;
  - it is declared in an installed package, or an installed package calls
    it: `dbt deps` would undo the edit, and a root `overrides:` would keep
    the old name;
  - its properties file holds an alias or a merge key anywhere, so that a
    name, a schema or an identifier may come from elsewhere;
  - the source already declares the new name;
  - a line must go into an entry in flow style (`{name: t}`), a JSON file
    included;
  - an edited YAML file would no longer parse, as when a flow collection
    spans lines that look like blocks, or a new name with `: ` goes into a
    plain `to:` value;
  - the new name is empty, or breaks the quotes it goes in, or, where a call
    sits in a YAML value such as a test's `to:`, is no plain identifier:
    that value may be a plain scalar, where `#`, `: `, `,` or a space at an
    edge would change it while the file still parses.
- Without a server, the proxy announces references and rename too, for
  sources and CTEs; on anything else they fail as other requests do.

## The last run as diagnostics

After the user's own `dbt build`, `run`, `test`, `seed` or `snapshot`, from
an editor, a terminal or an agent's shell, each node that errored and each
test that failed or warned shows as a diagnostic where it is written.  A
thread looks at `run_results.json` and `manifest.json` in the target path
(`DBT_TARGET_PATH` in the proxy's environment, else `target` under the
root; dbt 2.0.9 refuses `target-path` in `dbt_project.yml`) every second,
by size and time, and reads them when either changed, off the loop, a file
that does not read as JSON being read again when dbt changes it.  Only a
`build`, `run`, `test`, `seed` or `snapshot` replaces the failures, a
partial one included, as dbt's file does; any other command's results
(`compile`, `show`) leave them as they are.  Each result's id is looked up in the manifest there; an id it lacks
gives nothing.

- A node's error: Error at its file's first line, dbt's message.
- A test that failed: Error, `<test> failed: <n> rows`; warned: Warning;
  errored: dbt's message; a unit test that failed or errored: Error.  A
  generic test at its name under its node's entry in the properties file
  (a model, seed or snapshot; for a test on a source, the source its
  `depends_on` names and its table), then its column's (a quoted column's
  quotes left out; of a versioned model's columns, the one holding the
  test), a test of the entry itself and not of one it holds; else at the
  entry's name; a singular test at its file's first line; a unit test at
  its entry; a function at its file.  A package's node at its file in
  that package.
- `source: "dbt-lsp-plus"`, `code: "last-run"`, no `version`.

The places are found again in the file's text, open or on disk, at each
publication, its entries and tests read once.  A file the client changes
or saves after the run loses its failures, as does one whose file is newer
than the results; when the server has diagnostics for it, the proxy
publishes nothing then, since their ranges are of the text before the
edit, and the server's next publication goes out without the failures.  Per file, the
proxy keeps the server's last diagnostics and URI and the URI it last
published under: the server's notification passes byte for byte for a file
without failures, else the proxy publishes the server's diagnostics with
its own appended, under the server's URI, else the client's, else the
file's, first clearing a URI it published under before.  Nothing after
`shutdown`.

## Navigation from YAML

dbt lsp 2.0.9 answers null to definition and hover at every position of a
properties file, and to definition on a `source()` or `doc()` argument in
SQL.  The proxy answers a definition itself where the document alone shows
one of these targets, and forwards every other one, YAML included:

- `source`: a `source()` argument in any file, or a source's or table's
  `name:`, leads to the table's or source's `name:`;
- `ref`: a `ref()` argument in a YAML file (in SQL the server answers), or
  a unit test's `model:`, leads to the model's, seed's or snapshot's file;
  a `ref` that names a version is forwarded, since the scan knows no
  versions' files;
- `doc`: a `doc()` argument in a YAML file, its quotes as YAML escapes them,
  leads to the docs block; dbt has no `doc` in SQL;
- `entry`: the `name:` of a model, seed, snapshot, analysis or macro entry
  leads to its file or block;
- `test`: a generic test's name in a `data_tests:` or `tests:` list below
  the top level (an item, the one key of an item, or `test_name:`) leads
  to its test block; dbt's own tests have none, and get null.

Lookup follows dbt 2.0.9, probed with `dbt parse`: a package the name gives
is the only one searched; else a `ref` or a `source` tries the file's own
package (the installed package whose directory holds it), then the root,
then any, the root first under `flags:
require_ref_searches_node_package_before_root: false`; a `doc` tries the
root, then the file's package, then any, flag or not; an entry describes a
resource of the file's own package only, since dbt 2.0.9 calls an entry
naming another package's resource unused (dbt-core 1.12 patches it).  As
dbt-core 1.12.6's parser reads them: a generic test, the file's package
then the root; a unit test's model, the file's package.  The first package
that has something gives one location, or every
declaration of a source.  A location is the name of a block or of a
`name:`, or the start of a model's file, as dbt lsp answers a `ref`.

Hover there extends the hover summary (see "Node information and hover"):
on a `doc`, the docs block's markdown as written, if it holds any; on a
generic test, its opening tag and its package, in a code block for a
client that reads markdown; on an entry's `name:`, its
node's summary, which `nodeInfo` with that position gives whole.  Without a
server, the proxy announces `definitionProvider`, answers the same targets,
and gives null elsewhere rather than a failure.

## Within a document

The proxy adds four capabilities that need one document alone, and the
rename of a CTE, which the server answers with null:

- Outline: in SQL and Markdown, the Jinja blocks and the CTEs, nested by
  extent.  In a `.yml` or `.yaml` file, where dbt lsp 2.0.9 answers `[]`,
  the named entries of a properties file: those under `models`, `seeds`,
  `snapshots`, `analyses`, `macros`, `functions`, `data_tests` (or
  `tests`), `sources`, `exposures`, `metrics`, `semantic_models`,
  `saved_queries`, `unit_tests` and `groups`, with the `columns` of a
  model, seed, snapshot, analysis or source table, a model's `metrics`
  and the `tables` of a source.  The detail is what the entry declares
  (`model`, `table`, `semantic model`); the kinds are those agreed with
  dbt-mode.  An entry runs from its first key to its last content: the
  blank lines and the comment lines less deep than its first key that end
  it belong to what follows, while a key commented out and a block
  scalar's deeper lines stay in; in flow style it ends at its brace.  An
  entry without a name, such as one whose `name:` is being typed, is no
  symbol, and aliases are not followed.  Other YAML, such as
  `dbt_project.yml`, has no entry, nor does YAML that does not parse.
- Folding: every Jinja block (`if`, `for`, `call`, `filter`, `macro` and
  the others), CTEs and multi-line Jinja tags (`region`), and multi-line
  `{# #}` comments and, in SQL, `/* */` comments (`comment`), by line.  A
  closer alone at the start of its line stays visible, its whitespace
  control (`-}}`) included; a tag or comment left open runs to the end of
  the text.  Of the ranges that start on one line, the longest comes first,
  since some clients keep only the first.
- Highlights: on a CTE, its definition (write) and its uses (read); on a tag
  of a Jinja block, every tag of the block, `elif` and `else` included.
- Selection ranges: the word, the Jinja tag, a CTE's or block's content,
  the CTE or block, and the whole text, each containing the one before.
- Folding and selection ranges are for SQL and Markdown: on a YAML file the
  proxy answers null, so that a client that falls back on null, as VS Code
  does, folds and selects by indentation; one that does not gets nothing,
  as it got before the proxy announced folding.
- CTE rename, in a SQL file: the definition and every use, whole words
  (whose neighbours are no letter, digit, `_` or `$`) outside Jinja,
  comments and strings, within the innermost block that defines the CTE,
  not after a `.`, matched without regard to case unless quoted.  A new
  name must be a plain identifier and not another CTE's.  A rename is
  refused when the name appears in a string inside a Jinja tag, as in
  `is_positive('orders.amount')`, since a macro may render it as SQL; the
  arguments of `ref` and `source`, which name models and sources, and the
  names of functions do not count.  A name a macro builds, as in `'ord' ~
  'ers'`, cannot be seen.
- Without a server, all of these still answer.

## Node information and hover

dbt lsp 2.0.9 exposes neither relations nor resolved configuration:
`dbt.listNodes` gives each node `config: {materialized, access, group}` and
no more, and `dbt.getCurrentNode` answered null to every argument tried.
So the proxy runs the user's own `dbt parse`.

### What `dbt parse` does, observed on 2.0.9

- With `--target-path` and `--log-path` under a cache, it writes its
  manifest, `semantic_manifest`, `snapshots/`, `generic_tests/` and logs
  there, and an empty `~/.dbt/leases/` in the home: 0.25 to 0.4 s on the
  fixture, 2.1 s on 2,000 models and 4,000 tests.  It runs beside a running
  `dbt lsp`.
- When `packages.yml` names a package that is not installed, it resolves
  packages as `dbt deps` does: online it writes `package-lock.yml` and
  rewrites `dbt_packages/`; offline it retries the hub for about 5 s.  No
  flag turns this off; `dbt lsp` does the same at its start.
- It sends telemetry on every run, even with `DO_NOT_TRACK=1`, unless given
  `--no-send-anonymous-usage-stats`, which `dbt lsp` takes too.
- Without `--profiles-dir`, profiles are looked up in the working
  directory, then `~/.dbt`, not in the project.
- A run that fails on a Jinja error, an unknown `ref` or invalid YAML exits
  1 and still writes a manifest of what it parsed; one that fails on the
  profile or an `env_var` writes none.
- The manifest's `relation_name` is quoted by the adapter, null for an
  ephemeral model and for tests, set for analyses and disabled nodes, which
  are never built; a disabled node is only under `disabled`; an owner is
  its group's; a source with no freshness has null counts under
  `warn_after` and `error_after`; a test on a source, or a singular test,
  has no `attached_node`, only `depends_on`; a composite `unique_key` is an
  array; each version of a versioned model is a node of its own.
- The server's hover on a `ref` is markdown with no range, starting with the
  description and the parents and children; on a `source()`, a column, a
  macro call or a comment it answers null.  The fake does not model this:
  each test that hovers scripts the server's answer.

### How the proxy runs it

- The command: the server command with its `lsp` word replaced by `parse`,
  every word before it kept (`uvx dbt`), then from after it only
  `--profiles-dir`, `--profile`, `--target`, `--vars` and
  `--packages-install-path`, in either form; always `--project-dir <root>`,
  `--target-path` and `--log-path` under the cache, `--write-json`,
  `--no-version-check`, `--no-send-anonymous-usage-stats`,
  `--no-manage-state` and `--quiet`.  A server command with no `lsp` word
  gets no parse.  The working directory and the environment are the
  proxy's.
- The child: no input; both outputs in `parse.log` in the cache, never on
  the client's channel; a failure names that file and gives dbt's first
  `[error]` line, past the banner it writes.  A thread waits on it and
  reports to the event loop; it is killed past two minutes, and at `exit`
  or when the client closes.  Only the direct child is killed: a launcher's
  grandchild may outlive it.
- When: lazily.  A change of the client's marks the index stale; a
  `nodeInfo` or a hover that finds it stale, or an inlay hint request that
  finds no index, starts a parse, and is answered from the last index
  meanwhile.  A parse starts only when the server has compiled since the
  change, so that dbt has resolved the packages itself, and such a request
  made before that compilation starts it once it comes,
  when every package is local or `package-lock.yml` is no older than the
  file naming the packages (an empty or comment-only file names none), with
  a server and before `shutdown`; one at a time.  When nothing has compiled
  since the change, the request makes the proxy list the graph, which makes
  dbt lsp compile: dbt lsp compiles only after a document opens, and an
  agent may ask for a node without opening one.  A reason a parse cannot
  start, or a failure, is logged once with `window/logMessage`, and never
  after `shutdown`.
- The cache: `$XDG_CACHE_HOME/dbt-lsp-plus/<hash of the root and the
  command>/<pid>/`, else under `~/.cache`, an empty or relative value being
  ignored; removed at exit; those beside it that no parse wrote for a day,
  as a proxy killed by a signal leaves, removed when the next one starts
  (asking whether a process lives would miss a home shared between hosts);
  the manifest is
  deleted before each run, so that only one this run wrote counts.
- The index: the manifest's nodes, sources, disabled nodes and groups, with
  `relation` (dbt's `relation_name`, null for an analysis) and `tests` (the
  ids attached).  A run that fails part of the way lays its entries over the
  last index.

### Inlay hints

dbt lsp 2.0.9 announces inlay hints and answers `[]` on a model, null on a
macro file and on a properties file.  The proxy forwards each request and
puts its own hints after the server's (appended to an array, alone in
place of null; an error passes as it came): after each `ref` or `source`
whose arguments are literals, right after its closing parenthesis, the
relation dbt builds for its node, from the last parse's index, or
`ephemeral` by the node's materialization; none for a disabled node, an
analysis, an unknown node, or a `ref` that a keyword argument follows,
whose version the index cannot tell.  Kind Type, padding on the left, the
node's unique id as tooltip.  Computed from the document's text and the
index when the request arrives, within `catch_unwind`, with no project
scan, each call's node found by its name.  A hint request starts a parse
only when there is no index yet, since clients ask on every edit and
scroll, and one made before dbt lsp's first compilation starts it once
that comes; after each parse that gives
an index, a client that declares `workspace.inlayHint.refreshSupport` is
asked to refresh, under an id of the proxy's own, whose answer stays
here.  The values of variables are not hinted yet: in dbt 2.0.9 a macro's
`var()` takes its caller's scope and string values are rendered.

### What the client gets

- `workspace/executeCommand` `dbt-lsp-plus.nodeInfo`, added to the
  server's `executeCommandProvider.commands`: arguments `[uniqueId]` or
  `[{uri, position}]`, a position naming the `ref` or `source` under it,
  or the entry whose `name:` is there,
  else the one node its file defines.  The result is the manifest's node,
  null for no node, and error -32803 with the reason until a first parse
  has finished; after that, the last index answers, and a later failure is
  only logged.  A position names the call under it in SQL inside Jinja, and
  in YAML or a Python model outside it.
- A hover, with inlay hints (see "Inlay hints"), is a method that is
  forwarded and augmented: on a `ref` or
  `source` argument, or an entry's `name:` in YAML, resolved from the
  document's text when the request arrives, the server's answer gets the
  node's summary first, then a rule;
  plain text gets a blank line instead, an array of marked strings the
  summary as its first item, a null answer the summary alone.  Elsewhere,
  or without an index, the answer passes byte for byte.  The summary is one
  line of what dbt builds and where, then one of what is set beyond the
  defaults: group and owner, access when not protected, an enforced
  contract, the version, tags, disabled, the number of tests.

## Failure modes

| Situation | Behaviour |
|-----------|-----------|
| `dbt lsp` cannot start | The proxy answers `initialize` itself with its own capabilities and incremental sync, warns once with `window/showMessage`, answers `shutdown`, answers null to a definition it finds no target for, and fails every other request with -32803 |
| `dbt lsp` exits during the session | Its pending requests fail once each with -32803 and its exit status, a pending `shutdown` gets null, and an error message is shown unless `shutdown` came first.  The proxy goes on alone, as above, on the scanned graph |
| `dbt lsp` exits before it answers `initialize` | The proxy answers `initialize` itself, once, as when the server cannot start; before the client's `initialize`, it sends nothing, and warns once after its answer |
| `dbt.listNodes` errs, is cancelled too often, or never answers | The graph keeps its last listing, else the scan |
| A YAML file is not valid | Its sources are left out |
| A body is not JSON | Forwarded unchanged |
| A header is malformed, or a body is longer than 64 MiB | The stream stops: the frame boundary is lost |
| `exit` | Forwarded; the server has two seconds to exit before it is killed, and a parse under way is killed.  The proxy exits 0 after `shutdown`, else 1 |
| `dbt parse` fails, or cannot start | The index keeps what it had, laid over by what a failed run still wrote; the reason is logged once, and given by `nodeInfo` until a first index exists |

The server is not restarted after a crash.

## Known limitations

- `docs-paths` is ignored: docs blocks are read from every resource path.
- JSON-RPC batches are forwarded as they are; a batched `exit` does not stop
  the proxy.  LSP clients do not send batches.
- A backslash-escaped quote in SQL, dollar-quoted strings, and `#` or `//`
  comments holding a quote can mislead the CTE scan.  So can a named window:
  in `window w1 as (...), w2 as (...)`, `w2` reads as a CTE.
- A backslash-escaped quote inside a Jinja string, as in `'it\'s'`, ends the
  string for the scan; an odd number of them hides the blocks after it.
- SQL files under `tests/fixtures`, and those under `tests/generic` without a
  test block, count as singular tests.
- A dispatch names the implementations of its namespace only: the root
  project's own, which dbt tries first, and a `dispatch:` search order in
  `dbt_project.yml` are not followed.
- Custom materializations are in the outline but not in workspace symbols.
- A new file not saved yet is not part of the project, nor are local packages
  declared in `packages.yml` outside `packages-install-path`, nor versioned
  models, Python models or YAML snapshots.
- A file that the project reaches through a link, opened by its real path
  outside the project, is not matched to its resource.
- A client that reuses the id of a listing the proxy abandoned after two
  minutes gets the server's late answer to that listing.  Only a client
  whose ids start with `dbt-lsp-plus/` can.
- Once the server has listed the graph, an unsaved edit of a `ref` in a node
  it listed shows in the call hierarchy only after the next save.
- In YAML, an apostrophe in a plain scalar, as in `description: It's the
  customers`, can unmask a later comment: a call commented out after it
  counts as a reference, and a rename edits it.
- `selectors.yml` and selectors on the command line name a source as
  `source:s.t`; a rename leaves them on the old name.
- A `ref` with no package names the root project's node first, then any,
  and a versioned model's latest version; a `ref` that names a version
  (`v=2`) still names the latest.
- Killing a parse kills `dbt` itself, or the launcher before it (`uvx`,
  `nix run`), whose child may then run on until it ends.
- A column or an alias spelt like a CTE counts as one of its uses, for its
  highlights and its rename.
- The last run's target path is the proxy's `DBT_TARGET_PATH`, not the
  shell's where the user ran dbt, and a run's `--target-path` is not seen.
  A failure stays until a new run, an edit of its file, or a save; on a
  file the server also reports, until the server's next publication.  Of
  two tests of one name on one entry, a failure goes to the first.  A
  local package linked into `dbt_packages` has its failures under the
  link's path, which an edit of the real file does not clear.  When the
  server names one file by two URIs, the proxy keeps the last.
- The outline shows a versioned model's columns, those of its versions
  included, as the model's.
- An inlay hint's node is looked up as the hover's, an enabled one first,
  then the root project, not the calling file's package as dbt 2.0.9 does
  for a `ref`.  In a Python model or YAML, a `#` comment between a `ref`'s
  last argument and its `)` hides its hint.
- `graph::resolve`, behind the call hierarchy, ignores `flags:
  require_ref_searches_node_package_before_root: false`, which navigation
  reads.
- A versioned model's `ref` (`v=2`) and its versions' files are not
  navigated; a generic test's arguments (`column_name:`, `to:` aside) and
  a `config:` block lead nowhere.
- Where one package has two files of a model's name, one of them disabled,
  a definition leads to the first by path, which may be the disabled one.
- A `doc` in a project that sets `docs-paths` to a directory outside its
  resource paths finds no docs block (see `docs-paths` above).
- A hover or a definition in a large properties file reads it several
  times: about 0.25 s per request on 2.8 MB of YAML, in a release build,
  on the loop that forwards every message.
- The outline of a YAML file that yaml-rust2 does not parse is empty: one
  in the middle of an edit, but also some that dbt reads, being laxer about
  indentation inside flow collections and quoted scalars, such as a flow
  sequence whose `]` sits at its key's column, a quoted description
  continued at its key's column, or a tab after a colon.  The scan of
  sources reads no table from such a file either.  The outline shows no
  tests nested under a resource, model versions, macro arguments, nor a
  semantic model's entities, dimensions and measures.

## Tests

- Unit tests for each pure module: framing, positions and incremental
  changes, the scan of SQL and Jinja, YAML places, the project, the graph
  and the shape of each answer.
- Integration tests run the binary in front of `fake-dbt-lsp`, a scripted
  server that behaves as 2.0.9 was seen to.  They cover:
  - byte-for-byte forwarding both ways: requests, notifications and answers
    of each side, and a body that is not JSON;
  - the capabilities added, and features left to a server that announces
    them;
  - the outline of unsaved edits in UTF-16, after a character that UTF-16
    and UTF-32 count apart, and in the server's UTF-8; flat for a client
    without hierarchy; a closed document's edits dropped; the outline of a
    properties file, flat and nested, and none for `dbt_project.yml`;
  - workspace symbols, and the call hierarchy through the proxy's own ids;
    the ids of the client's that the proxy skips or refuses;
  - the root: `--project-dir` first and as spelt, the client's folder or the
    working directory without it, the client's spelling through a link, a
    link inside the project, a document opened through another spelling;
  - listings: on the first `didOpen` only, after the change that prompts
    them, no loop, at most three retries, none while the client's is pending
    nor after `shutdown`, an empty `*` and failed or cancelled listings
    keeping the graph;
  - no server, warned once; a server that exits at once, a crash with
    several requests pending, the scanned graph after a crash, a crash
    before `initialize` and during `shutdown`, a server killed when it
    ignores `exit`, and `exit` without `shutdown`;
  - references and renames of sources, and every other one left to the
    server; files the scan does not hold; the client's URI of an open
    document; the server's UTF-8; without a server; the capability that
    tells a client which requests the proxy answers itself;
  - node information and the hover: no parse before the server compiles,
    with packages that need the network, or without a server; one parse at
    a time and one more after a change during it; the parse's flags and
    cache; dbt's output kept off the client's channel; a parse killed at
    `exit`; a node by id or by position; the summary first on a hover of a
    ref, any other hover byte for byte.
  - the last run: failures where written, the server's diagnostics joined
    and its others byte for byte, a new run replacing, a `compile`
    keeping, an edit clearing, a change of URI clearing the old one,
    nothing after `shutdown`;
  - inlay hints: after the server's, alone for its null, its error byte
    for byte; a first parse from a hint request, none more once an index
    exists; the refresh asked of a client that can, its answer kept from
    the server;
  - navigation from YAML: each kind of target and its location, dbt's own
    test null, the server never asked; a column's name, `dbt_project.yml`
    and a SQL `ref` forwarded; a `source()` in SQL; hover on a doc, a test
    and an entry's name; nodeInfo on an entry's name; without a server.
- One opt-in live test runs the real `dbt lsp` on a copy of the fixture,
  from an empty environment with a scratch home, in the copy, and checks
  what this note records of `dbt.listNodes` and of `positionEncoding`:
  `DBT_LSP_PLUS_TEST_DBT=/path/to/dbt cargo test --test proxy -- --ignored`.
  The fake also asks for the configuration after `initialized` and answers
  an empty listing with `{"error": "No nodes found"}`, as 2.0.9 was seen to.
  The live rename also relies on two observations that no fake-backed test
  needs, so the fake leaves them out: `dbt.compileFile` answers
  `{"file_uri": ...}` and writes the compiled SQL there, where a file from
  an earlier compilation may linger; and the server reads a properties file
  again after `workspace/didChangeWatchedFiles`.

## What it would take to finish

In the order of the feature list agreed with the dbt-mode session:

1. Source references and rename (A4): done in plan 002.
2. Resolved configuration and the hover summary (A5, A20): done in plan
   004.
3. Folding ranges (A7), with highlights, selection ranges and CTE rename:
   done in plan 003.
4. A cache of the scan, keyed by path, modification time and size.  The
   review of plan 001 measured about 300 ms per scan on 1,500 models with
   13 MB of YAML properties, every YAML file being parsed whole for its
   `sources:`; the 40 ms above had no YAML.
5. YAML outline (A1): done in plan 005.
6. Restart of a crashed server, replaying `initialize`, `initialized` and
   the open documents.
7. `packages.yml` local packages outside `packages-install-path`, versioned
   models (`ref('m', v=2)`), Python models, YAML snapshots.
8. A run in an editor, then with refmux in front of the server for
   monorepos (one root per proxy today).
