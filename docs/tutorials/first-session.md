# Your first session

This tutorial builds dbt-lsp-plus, runs it from Emacs on the small dbt
project that its tests use, `tests/fixtures/shop`, and shows two of its
features: the outline of a model and its call hierarchy.

You need:

- Rust 1.89 or later and a C linker, or Nix, whose `nix develop` gives
  Rust;
- Emacs 31, whose Eglot shows a call hierarchy;
- a dbt 2 on `PATH` as `dbt`: dbt-oss, or dbt Labs' build.

## Build the server

```
git clone https://github.com/piotr-yuxuan/dbt-lsp-plus
cd dbt-lsp-plus
nix develop
cargo build --release
export PATH="$PWD/target/release:$PATH"
dbt-lsp-plus --version
```

The last command prints `dbt-lsp-plus` and its version.

## Copy the project

dbt may write `target` and `logs` in a project it runs in, so work on a
copy:

```
cp -r tests/fixtures/shop /tmp/shop
```

`shop` has three staging models, three marts, two seeds, a snapshot,
macros and a local package, on DuckDB.

## Tell Emacs about the server

Write `/tmp/shop.el`:

```elisp
;;; -*- lexical-binding: t -*-
(require 'eglot)
(add-to-list 'eglot-server-programs
             '(sql-mode . ("dbt-lsp-plus" "--" "dbt" "lsp"
                           "--project-dir" "/tmp/shop"
                           "--profiles-dir" "/tmp/shop")))
```

The words after `--` are the server command.  `--profiles-dir` names the
directory of `profiles.yml`, which dbt does not look for in the project.

Then open a mart, with no configuration of your own:

```
emacs -Q -l /tmp/shop.el /tmp/shop/models/marts/orders.sql
```

and type `M-x eglot RET`.  The echo area says that the server
`dbt-lsp-plus` now manages `sql-mode` buffers.

## See the outline

Type `M-x imenu RET` and then `TAB`.  The completions are the three CTEs
of `orders.sql`: `orders`, `customers` and `final`.  Pick `final`: point
moves to its definition.

The outline is the server's `textDocument/documentSymbol`, read from the
buffer as it stands, unsaved edits included.

## See the call hierarchy

Put point on the last line, `select * from final`, and type `M-x
eglot-show-call-hierarchy RET`.  A buffer shows the model `orders`, what
uses it (`←`) and what it uses (`→`):

```
[-]  ∘ orders
 ├─[+]  ← order_counts
 ├─[+]  ← customers
 ├─[+]  ← assert_positive_amount
 ├─[+]  → is_positive
 ├─[+]  → stg_customers
 └─[+]  → stg_orders
```

`order_counts` is an analysis, `assert_positive_amount` a singular test,
and `is_positive` a macro of the local package `shop_utils`.  Click a `[+]`
to unfold the next level.

One model is missing: `order_summary` selects from
`ref(var('summary_upstream'))`, and only dbt knows that the variable names
`orders`.  Wait a few seconds and show the hierarchy again:
`← order_summary` is now listed.  The server took the dependencies from
dbt itself, from its own `dbt parse`.

Now go back to `orders.sql`, put point on `stg_orders` in
`ref('stg_orders')`, and show the hierarchy again.  It is that of
`stg_orders`: used by `orders` and the snapshot `orders_snapshot`, and
using the seed `raw_orders` and the macro `cents_to_dollars`.

## What next

- [Configure an editor](../how-to/configure-an-editor.md) for your own
  projects.
- [The features](../reference/features.md), method by method.
- [Configure an editor, with either build of dbt
  2](../how-to/configure-an-editor.md#with-either-build-of-dbt-2), and
  what that leaves out.
