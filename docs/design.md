# Design of dbt-lsp-plus

dbt-lsp-plus sits between any LSP client and the proprietary `dbt lsp` of
dbt 2 (observed on 2.0.9).  It forwards every message unchanged and answers
itself the requests of the features that the server does not announce.  An
editor gets them through standard LSP, and so does an agent whose LSP tool
speaks definition, references, hover, document and workspace symbols and the
call hierarchy.

## Boundary

- It uses only the server's interface: its messages, its announced
  capabilities and its custom command `dbt.listNodes`.
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
  requests of an owned method, which the proxy answers itself.
- Server to client: every frame is forwarded as it arrived, except:
  - the answer to `initialize`, to which the proxy adds the capabilities
    the server lacks (`documentSymbolProvider`, `workspaceSymbolProvider`,
    `callHierarchyProvider`), and whose `positionEncoding` it adopts
    (UTF-16 when absent, as with 2.0.9);
  - the answers to the proxy's own requests, which never reach the client.
- References and renames stay the server's, but on a source the server
  answers them with nothing: the proxy answers those requests itself, when
  the position is on a `source()` argument or on a source's or table's
  `name:` under `sources:`, and forwards every other one.
- A method is owned only when the server does not announce it.  dbt lsp
  2.0.9 announces `documentSymbolProvider: false` but answers that request
  with a flat list of tokens, so the proxy owns it.  A later server that
  announces a feature gets its requests back.
- Request ids: the client's ids and the server's ids pass unchanged.  The
  proxy's own requests carry string ids `dbt-lsp-plus/N`, never one that a
  pending request of the client's carries; an answer is the proxy's own only
  when its id has that prefix and answers no request of the client's.
- A panic while the proxy answers a request fails that request with -32603;
  the session goes on.

## Root

The project directory is the server command's `--project-dir` (`--project-dir
X` or `--project-dir=X`), so that the proxy reads the project the server
runs on; else the client's first workspace folder or `rootUri`, when it holds
a `dbt_project.yml`; else the working directory.  When the client's folder
is the same directory as `--project-dir` under another spelling (a symbolic
link), the client's spelling wins, so that the URIs the proxy returns are
the ones the client opened.  A document reached through another spelling is
mapped onto the root by its resolved path.

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
    place of each scalar as written;
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
- The graph: edges between nodes come from `dbt.listNodes` once the server
  has answered, since only dbt resolves `ref(var('x'))`; until then, from the
  scan.  The server lists no macros, so the edges to macros always come from
  the scan; a dispatch names every `*__name` implementation of its package.
  A listing that fails keeps the graph it had.  An empty `*` listing counts
  as a failure, since the scan found resources; an empty `source:*` does not.

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
- the first `didOpen`;
- `didSave`;
- `workspace/didChangeWatchedFiles`, and files created, renamed or deleted.

The server gets the client's change before the listing it prompts.  The
proxy never lists while a `dbt.listNodes` of the client's is pending, nor
sends the second selector then, retries a cancelled listing at most three
times, and abandons one unanswered after two minutes, on time, even when the
client is quiet.

## Sources

- References of a source table, or of a whole source: every `source()`
  call that names it, in SQL inside Jinja tags and in YAML without them (a
  relationships test's `to:`), and its declarations when the client asks.
- A rename keeps the relation in the warehouse.  dbt takes a table's
  identifier from its name, and a source's schema from its name, unless they
  are set.  So renaming a table without `identifier:` inserts `identifier:
  <old>` after its `name:` line, at the key's indentation, and renaming a
  source without `schema:` inserts `schema: <old>`.  The live test renames
  `events.page_views` and checks that dbt still compiles the model that
  selects from it to `page_views`.
- A name inside quotes is replaced inside them, and must not hold that quote
  or a backslash.  An unquoted YAML value is written plain when it is a plain
  identifier, else double-quoted.
- A rename is refused, never applied in part, when the source is declared
  nowhere, or in several packages, or in flow style (`{name: t}`), where no
  line can go.
- Without a server, the proxy announces references and rename too, for
  sources; on anything else they fail as other requests do.

## Within a document

The proxy adds three capabilities that need one document alone, and the
rename of a CTE, which the server answers with null:

- Folding: blocks and CTEs, multi-line Jinja tags (`region`) and multi-line
  `{# #}` and `/* */` comments (`comment`), by line.  A closer alone at the
  start of its line stays visible.
- Highlights: on a CTE, its definition (write) and its uses (read); on a tag
  of a Jinja block, every tag of the block, `elif` and `else` included.
- Selection ranges: the word, the Jinja tag, a CTE's or block's content,
  the CTE or block, and the whole text, each containing the one before.
- CTE rename, in a SQL file: the definition and every use, whole words
  outside Jinja, comments and strings, not after a `.`, matched without
  regard to case unless quoted.  A new name must be a plain identifier and
  not another CTE's.

## Failure modes

| Situation | Behaviour |
|-----------|-----------|
| `dbt lsp` cannot start | The proxy answers `initialize` itself with its own capabilities and incremental sync, warns once with `window/showMessage`, answers `shutdown`, and fails every other request with -32803 |
| `dbt lsp` exits during the session | Its pending requests fail once each with -32803 and its exit status, a pending `shutdown` gets null, and an error message is shown unless `shutdown` came first.  The proxy goes on alone, as above |
| `dbt lsp` exits before it answers `initialize` | The proxy answers `initialize` itself, once, as when the server cannot start |
| `dbt.listNodes` errs, is cancelled too often, or never answers | The graph keeps its last listing, else the scan |
| A YAML file is not valid | Its sources are left out |
| A body is not JSON | Forwarded unchanged |
| A header is malformed, or a body is longer than 64 MiB | The stream stops: the frame boundary is lost |
| `exit` | Forwarded; the server has two seconds to exit before it is killed.  The proxy exits 0 after `shutdown`, else 1 |

The server is not restarted after a crash.

## Known limitations

- `docs-paths` is ignored: docs blocks are read from every resource path.
- JSON-RPC batches are forwarded as they are; a batched `exit` does not stop
  the proxy.  LSP clients do not send batches.
- A backslash-escaped quote in SQL, dollar-quoted strings and `#` comments
  can mislead the CTE scan.
- SQL files under `tests/fixtures` count as singular tests.
- A new file not saved yet is not part of the project, nor are local packages
  declared in `packages.yml` outside `packages-install-path`, nor versioned
  models, Python models or YAML snapshots.
- Once the server has listed the graph, an unsaved edit of a `ref` shows in
  the call hierarchy only after the next save.
- A column or an alias spelt like a CTE counts as one of its uses, for its
  highlights and its rename.

## Tests

- Unit tests for each pure module: framing, positions and incremental
  changes, the scan of SQL and Jinja, YAML places, the project, the graph
  and the shape of each answer.
- Integration tests run the binary in front of `fake-dbt-lsp`, a scripted
  server that behaves as 2.0.9 was seen to.  They cover:
  - byte-for-byte forwarding both ways, the server's requests and the
    client's answers included, and a body that is not JSON;
  - the capabilities added, and a feature left to a server that announces it;
  - the outline of unsaved edits in UTF-16 and in the server's UTF-8, flat
    for a client without hierarchy;
  - workspace symbols, and the call hierarchy through the proxy's own ids; a
    client id with the proxy's prefix;
  - the root: `--project-dir` first, the client's spelling through a link;
  - listings: no loop, at most three retries, none while the client's is
    pending, a failed one keeping the graph;
  - no server, a crash with several requests pending, a crash before
    `initialize` and during `shutdown`, and `exit` without `shutdown`.
- One opt-in live test runs the real `dbt lsp` on a copy of the fixture,
  from an empty environment with a scratch home, in the copy:
  `DBT_LSP_PLUS_TEST_DBT=/path/to/dbt cargo test --test live -- --ignored`.

## What it would take to finish

In the order of the feature list agreed with the dbt-mode session:

1. Source references and rename (A4): done in plan 002.
2. Resolved configuration (A5, A20): `dbt parse --profiles-dir <user's>
   --target-path <cache> --log-path <cache>/logs`, debounced on save (about
   half a second on a small project).  Then a command `dbtPlus/nodeInfo` that
   returns the whole node, and a hover that summarises it, merged after the
   server's own hover.  This is the first feature that needs a request to
   wait for the server, so the event loop must hold a client request until
   the server answers.
3. Folding ranges (A7), with highlights, selection ranges and CTE rename:
   done in plan 003.
4. A cache of the scan, keyed by path, modification time and size, if a
   real project shows the 40 ms above to grow too much.
5. YAML outline (A1): models, sources, columns, exposures.
6. Restart of a crashed server, replaying `initialize`, `initialized` and
   the open documents.
7. `packages.yml` local packages outside `packages-install-path`, versioned
   models (`ref('m', v=2)`), Python models, YAML snapshots.
8. A run in an editor, then with refmux in front of the server for
   monorepos (one root per proxy today).
