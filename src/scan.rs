// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! What one file of SQL and Jinja holds: its blocks, its CTEs and its calls.
//! Offsets are bytes into the file's text.  Every function here is pure.

use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Macro,
    Test,
    Snapshot,
    Materialization,
    Docs,
    Set,
    Cte,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Symbol {
    pub kind: Kind,
    pub name: String,
    pub range: Range<usize>,
    pub name_range: Range<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Ref {
        package: Option<String>,
        name: String,
    },
    Source {
        source: String,
        table: String,
    },
    Macro {
        package: Option<String>,
        name: String,
    },
    /// `adapter.dispatch('name', 'package')`: the macros `*__name` of the package.
    Dispatch {
        package: Option<String>,
        name: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub target: Target,
    pub range: Range<usize>,
    /// The content of each string argument of a `ref` or a `source`, quotes excluded.
    pub arguments: Vec<Range<usize>>,
}

/// Replaces with spaces the bytes of RANGE in OUT, newlines kept.
fn blank(out: &mut [u8], range: Range<usize>) {
    out[range].iter_mut().filter(|b| **b != b'\n').for_each(|b| *b = b' ');
}

/// Blanks each span that one of SPANS opens: (opener, closer, blank).  A
/// span with blank false, such as a string, is skipped whole, so that an
/// opener inside it does not count.  The text keeps its length, and its
/// offsets their meaning.
fn mask(text: &str, spans: &[(&str, &str, bool)]) -> String {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut at = 0;
    while at < bytes.len() {
        let Some((open, close, blanks)) = spans.iter().find(|(open, _, _)| bytes[at..].starts_with(open.as_bytes()))
        else {
            at += 1;
            continue;
        };
        let from = at + open.len();
        let end = text[from..].find(close).map_or(text.len(), |found| from + found + close.len());
        if *blanks {
            blank(&mut out, at..end);
        }
        at = end;
    }
    // Every span starts and ends at an ASCII delimiter, so whole characters are blanked.
    String::from_utf8(out).expect("masking blanks whole characters")
}

/// TEXT without its Jinja comments, the only comments Jinja honours: a tag
/// inside a SQL comment is rendered all the same.
fn mask_jinja_comments(text: &str) -> String {
    mask(text, &[("{#", "#}", true)])
}

static RAW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A\{%-?\s*raw\s*-?%\}\z").unwrap());
static END_RAW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{%-?\s*endraw\s*-?%\}").unwrap());

/// The end of a Jinja tag whose content starts at FROM: past the first CLOSE
/// followed by `}` outside strings and nested braces, else the end of BYTES.
fn tag_end(bytes: &[u8], from: usize, close: u8) -> usize {
    let (mut quote, mut depth) = (None, 0usize);
    for at in from..bytes.len() {
        let byte = bytes[at];
        if let Some(open) = quote {
            quote = (byte != open).then_some(open);
            continue;
        }
        if depth == 0 && byte == close && bytes.get(at + 1) == Some(&b'}') {
            return at + 2;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    bytes.len()
}

/// The Jinja tags of MASKED, whose comments are blanked: each `{{ }}` and
/// `{% %}`.  The content of a raw block holds none.
fn jinja_tags(masked: &str) -> Vec<Range<usize>> {
    let bytes = masked.as_bytes();
    let mut tags = Vec::new();
    let mut at = 0;
    while let Some(open) = masked[at..].find('{').map(|found| at + found) {
        let close = match bytes.get(open + 1) {
            Some(b'{') => b'}',
            Some(b'%') => b'%',
            _ => {
                at = open + 1;
                continue;
            }
        };
        let end = tag_end(bytes, open + 2, close);
        tags.push(open..end);
        at = end;
        if RAW.is_match(&masked[open..end]) {
            let raw_end = END_RAW.find(&masked[end..]).map(|tag| end + tag.start()..end + tag.end());
            at = raw_end.as_ref().map_or(bytes.len(), |tag| tag.end);
            tags.extend(raw_end);
        }
    }
    tags
}

/// TEXT as SQL's parentheses see it: without Jinja comments and tags, SQL
/// comments and SQL strings.  Double-quoted identifiers are kept.
fn mask_code(text: &str) -> String {
    let masked = mask_jinja_comments(text);
    let mut out = masked.clone().into_bytes();
    jinja_tags(&masked).into_iter().for_each(|tag| blank(&mut out, tag));
    let without_tags = String::from_utf8(out).expect("masking blanks whole characters");
    mask(&without_tags, &[("--", "\n", true), ("/*", "*/", true), ("'", "'", true), ("\"", "\"", false)])
}

static BLOCK_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)\A\{%-?\s*(end)?(macro|test|snapshot|materialization|docs|set)\b(.*?)-?%\}\z").unwrap()
});
/// The keyword of each block that BLOCK_TAG finds.
const BLOCK_KINDS: [(&str, Kind); 6] = [
    ("macro", Kind::Macro),
    ("test", Kind::Test),
    ("snapshot", Kind::Snapshot),
    ("materialization", Kind::Materialization),
    ("docs", Kind::Docs),
    ("set", Kind::Set),
];
static BLOCK_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A\s*([\p{L}_][\p{L}\p{N}_]*)").unwrap());

/// The Jinja blocks of TEXT: macro, test, snapshot, materialization, docs and
/// the block form of set.  A block left open runs to the end of the text.
pub fn blocks(text: &str) -> Vec<Symbol> {
    let masked = mask_jinja_comments(text);
    let mut open: Vec<Symbol> = Vec::new();
    let mut done = Vec::new();
    for range in jinja_tags(&masked) {
        let Some(tag) = BLOCK_TAG.captures(&masked[range.clone()]) else { continue };
        let Some(&(_, kind)) = BLOCK_KINDS.iter().find(|(keyword, _)| *keyword == &tag[2]) else { continue };
        if tag.get(1).is_some() {
            if let Some(at) = open.iter().rposition(|symbol| symbol.kind == kind) {
                let mut symbol = open.remove(at);
                symbol.range.end = range.end;
                done.push(symbol);
            }
            continue;
        }
        let rest = tag.get(3).unwrap();
        let block_set = kind != Kind::Set || !rest.as_str().contains('=');
        if let (true, Some(name)) = (block_set, BLOCK_NAME.captures(rest.as_str())) {
            let name = name.get(1).unwrap();
            let name_range = range.start + rest.start() + name.start()..range.start + rest.start() + name.end();
            open.push(Symbol { kind, name: name.as_str().to_owned(), range: range.start..text.len(), name_range });
        }
    }
    done.extend(open);
    done.sort_by_key(|symbol| symbol.range.start);
    done
}

static CTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?:\bwith(?:\s+recursive)?|,)\s*([\p{L}_][\p{L}\p{N}_$]*|"[^"]+")(?:\s*\([^()]*\))?\s+as\s*(?:(?:not\s+)?materialized\s*)?\("#,
    )
    .unwrap()
});

/// The CTEs of TEXT: each `name as (...)` after `with` or a comma, from its
/// name to its closing parenthesis, nested WITH clauses included.  A CTE
/// that Jinja generates is not one, but those after it are.
pub fn ctes(text: &str) -> Vec<Symbol> {
    let code = mask_code(text);
    let bytes = code.as_bytes();
    let symbol = |head: regex::Captures| {
        let name = head.get(1).unwrap();
        let open = head.get(0).unwrap().end() - 1;
        let mut depth = 0usize;
        let close = (open..bytes.len()).find(|&i| {
            depth = match bytes[i] {
                b'(' => depth + 1,
                b')' => depth - 1,
                _ => depth,
            };
            depth == 0
        });
        let end = close.map_or(text.len(), |close| close + 1);
        Symbol {
            kind: Kind::Cte,
            name: text[name.range()].to_owned(),
            range: name.start()..end,
            name_range: name.range(),
        }
    };
    CTE.captures_iter(&code).map(symbol).collect()
}

/// The blocks of TEXT, and its CTEs when it is SQL, ordered by start.
pub fn outline(text: &str, sql: bool) -> Vec<Symbol> {
    let mut symbols = blocks(text);
    if sql {
        symbols.extend(ctes(text));
    }
    symbols.sort_by_key(|symbol| (symbol.range.start, std::cmp::Reverse(symbol.range.end)));
    symbols
}

static REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bref\s*\(\s*['"]([^'"]*)['"](?:\s*,\s*['"]([^'"]*)['"])?"#).unwrap());
static SOURCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bsource\s*\(\s*['"]([^'"]*)['"]\s*,\s*['"]([^'"]*)['"]"#).unwrap());
static DISPATCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"\badapter\s*\.\s*dispatch\s*\(\s*['"]([^'"]+)['"](?:\s*,\s*(?:macro_namespace\s*=\s*)?['"]([^'"]+)['"])?"#,
    )
    .unwrap()
});
static MACRO_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:\b(macro|test|materialization)\s+)?(?:\b([A-Za-z_][A-Za-z0-9_]*)\s*\.\s*)?\b([A-Za-z_][A-Za-z0-9_]*)\s*\(",
    )
    .unwrap()
});

/// The `ref` and `source` calls of TEXT, which starts at offset AT of its file.
fn refs_and_sources(text: &str, at: usize) -> Vec<Call> {
    let shift = |found: regex::Match| at + found.start()..at + found.end();
    let mut found = Vec::new();
    for call in REF.captures_iter(text) {
        let arguments = [call.get(1), call.get(2)].into_iter().flatten().map(shift).collect();
        let (package, name) = match call.get(2) {
            Some(name) => (Some(call[1].to_owned()), name.as_str().to_owned()),
            None => (None, call[1].to_owned()),
        };
        found.push(Call { target: Target::Ref { package, name }, range: shift(call.get(0).unwrap()), arguments });
    }
    for call in SOURCE.captures_iter(text) {
        let target = Target::Source { source: call[1].to_owned(), table: call[2].to_owned() };
        let arguments = vec![shift(call.get(1).unwrap()), shift(call.get(2).unwrap())];
        found.push(Call { target, range: shift(call.get(0).unwrap()), arguments });
    }
    found
}

/// The `ref`, `source`, `adapter.dispatch` and possible macro calls inside
/// the Jinja tags of TEXT, a tag inside a SQL comment included, since Jinja
/// renders it.  A possible macro call is a `name(` or `package.name(` outside
/// strings and not a method or a filter: which of them are macros is the
/// project's to decide.
pub fn calls(text: &str) -> Vec<Call> {
    let masked = mask_jinja_comments(text);
    let mut found = Vec::new();
    for range in jinja_tags(&masked) {
        let (tag, at) = (&masked[range.clone()], range.start);
        found.extend(refs_and_sources(tag, at));
        for call in DISPATCH.captures_iter(tag) {
            let target =
                Target::Dispatch { package: call.get(2).map(|p| p.as_str().to_owned()), name: call[1].to_owned() };
            let range = at + call.get(0).unwrap().start()..at + call.get(0).unwrap().end();
            found.push(Call { target, range, arguments: Vec::new() });
        }
        let code = mask(tag, &[("'", "'", true), ("\"", "\"", true)]);
        for call in MACRO_CALL.captures_iter(&code).filter(|call| call.get(1).is_none()) {
            let name = call.get(3).unwrap();
            let start = call.get(2).unwrap_or(name).start();
            if code[..start].trim_end().ends_with(['.', '|']) {
                continue;
            }
            let target =
                Target::Macro { package: call.get(2).map(|p| p.as_str().to_owned()), name: name.as_str().to_owned() };
            found.push(Call { target, range: at + start..at + name.end(), arguments: Vec::new() });
        }
    }
    found.sort_by_key(|call| call.range.start);
    found
}

/// The `ref` and `source` calls of a YAML properties file, which dbt reads
/// without Jinja delimiters, as in a relationships test's `to:`.  Comments
/// are left out.
pub fn yaml_calls(text: &str) -> Vec<Call> {
    let masked = mask(text, &[("'", "'", false), ("\"", "\"", false), ("#", "\n", true)]);
    let mut found = refs_and_sources(&masked, 0);
    found.sort_by_key(|call| call.range.start);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(symbols: &[Symbol]) -> Vec<(Kind, &str)> {
        symbols.iter().map(|symbol| (symbol.kind, symbol.name.as_str())).collect()
    }

    const MODEL: &str = "\
{# it's a comment: with fake as ( #}
-- with commented as (
with orders as (
    select * from {{ ref('stg_orders') }} where note = 'with x as ('
),

\"Customers\" (id) as materialized (
    select id from {{ source('raw', 'customers') }}
    where id in (select 1)
),
final as (
    with inner_cte as (select 1)
    select {{ cents_to_dollars('amount') }}, {{ shop_utils.is_positive('a') }}
    from orders
)
select * from final
";

    #[test]
    fn finds_ctes_and_nested_ctes_but_not_in_comments_or_strings() {
        let found = ctes(MODEL);
        assert_eq!(
            names(&found),
            [(Kind::Cte, "orders"), (Kind::Cte, "\"Customers\""), (Kind::Cte, "final"), (Kind::Cte, "inner_cte")]
        );
        let orders = &found[0];
        assert_eq!(&MODEL[orders.name_range.clone()], "orders");
        assert!(MODEL[orders.range.clone()].ends_with("x as ('\n)"));
        let final_cte = &MODEL[found[2].range.clone()];
        assert!(final_cte.starts_with("final as (") && final_cte.ends_with("from orders\n)"));
    }

    #[test]
    fn names_may_hold_any_letter() {
        let text = "{% macro métrique() %}{% endmacro %}with résultat as (select 1), 総計 as (select 2) select 1";
        assert_eq!(
            names(&outline(text, true)),
            [(Kind::Macro, "métrique"), (Kind::Cte, "résultat"), (Kind::Cte, "総計")]
        );
    }

    #[test]
    fn a_cte_left_open_runs_to_the_end() {
        let text = "with a as (select 1";
        assert_eq!(ctes(text)[0].range, 5..text.len());
    }

    const MACROS: &str = "\
{% macro cents_to_dollars(column, scale=2) -%}
  {% set fmt %}numeric{% endset %}{% set x = 1 %}
  ({{ column }} / 100)::{{ fmt }}
{%- endmacro %}
{#- {% macro commented() %}{% endmacro %} #}
{% test is_even(model, column_name) %}select 1{% endtest %}
{% snapshot orders_snapshot %}select 1{% endsnapshot %}
{% docs order_status %}The status.{% enddocs %}
{% materialization table_plus, adapter='duckdb' %}{% endmaterialization %}
{% macro unfinished() %}
";

    #[test]
    fn finds_jinja_blocks_with_their_extent() {
        let found = blocks(MACROS);
        assert_eq!(
            names(&found),
            [
                (Kind::Macro, "cents_to_dollars"),
                (Kind::Set, "fmt"),
                (Kind::Test, "is_even"),
                (Kind::Snapshot, "orders_snapshot"),
                (Kind::Docs, "order_status"),
                (Kind::Materialization, "table_plus"),
                (Kind::Macro, "unfinished"),
            ]
        );
        let macro_text = &MACROS[found[0].range.clone()];
        assert!(macro_text.starts_with("{% macro") && macro_text.ends_with("{%- endmacro %}"));
        assert_eq!(&MACROS[found[0].name_range.clone()], "cents_to_dollars");
        assert_eq!(found[6].range.end, MACROS.len());
    }

    #[test]
    fn a_block_end_inside_a_sql_comment_still_ends_the_block() {
        // Jinja renders before SQL is read: `--` hides nothing from it.
        let text = "{% macro m() %}\n  select 1 -- the end {% endmacro %}\n{% macro n() %}select 2{% endmacro %}\n";
        let found = blocks(text);
        assert_eq!(names(&found), [(Kind::Macro, "m"), (Kind::Macro, "n")]);
        assert!(text[found[0].range.clone()].ends_with("the end {% endmacro %}"));
    }

    #[test]
    fn markdown_prose_does_not_hide_a_docs_block_end() {
        let text = "{% docs paths %}Models live in models/*.sql; it's \"simple\".{% enddocs %}\n{% docs other %}x{% enddocs %}";
        assert_eq!(names(&blocks(text)), [(Kind::Docs, "paths"), (Kind::Docs, "other")]);
    }

    #[test]
    fn a_raw_block_and_nested_braces_hide_no_tag() {
        let text =
            "{{ config(meta={'a': {'b': 1}}) }}{% raw %}{% macro fake() %}{% endraw %}{% macro real() %}{% endmacro %}";
        assert_eq!(names(&blocks(text)), [(Kind::Macro, "real")]);
        let code = mask_code("{{ config(meta={'a': {'b': '}}'}}) }}with a as (select 1)");
        assert!(code.ends_with("with a as (select 1)"), "{code}");
    }

    #[test]
    fn an_apostrophe_inside_double_quotes_opens_no_string() {
        let text = "with a as (\n  select x as \"Customer's total\" from t where n = \"O'Brien\"\n),\nb as (select 1)\nselect * from b\n";
        assert_eq!(names(&ctes(text)), [(Kind::Cte, "a"), (Kind::Cte, "b")]);
    }

    #[test]
    fn ctes_after_a_jinja_generated_cte_are_found() {
        let text = "with {{ generated_cte('x') }},\nreal as (select 1),\n{% for c in cs %}{{ c }} as (select 2),{% endfor %}\nlast as (select 3)\nselect * from last";
        assert_eq!(names(&ctes(text)), [(Kind::Cte, "real"), (Kind::Cte, "last")]);
    }

    #[test]
    fn finds_calls_in_tags_even_inside_sql_comments_but_not_in_prose() {
        let text = "-- prose ref('prose') and {{ ref('live') }}\n{{ ref(\"pkg\", 'b') }} {{ source('raw', 'orders') }}\n\
                    {% macro m() %}{{ shop_utils.is_positive(x) }}{{ 'a,b'.split(',') }}{{ x | upper() }}{% endmacro %}\n\
                    {{ return(adapter.dispatch('safe_add', macro_namespace='shop')(a, b)) }}";
        let targets: Vec<Target> = calls(text).into_iter().map(|call| call.target).collect();
        let named = |package: Option<&str>, name: &str| Target::Macro {
            package: package.map(str::to_owned),
            name: name.to_owned(),
        };
        assert_eq!(
            targets,
            [
                Target::Ref { package: None, name: "live".into() },
                named(None, "ref"),
                Target::Ref { package: Some("pkg".into()), name: "b".into() },
                named(None, "ref"),
                Target::Source { source: "raw".into(), table: "orders".into() },
                named(None, "source"),
                named(Some("shop_utils"), "is_positive"),
                named(None, "return"),
                Target::Dispatch { package: Some("shop".into()), name: "safe_add".into() },
                named(Some("adapter"), "dispatch"),
            ]
        );
        let package_call = calls(text)
            .into_iter()
            .find(|c| matches!(&c.target, Target::Macro { package: Some(p), .. } if p == "shop_utils"))
            .unwrap();
        assert_eq!(&text[package_call.range], "shop_utils.is_positive");
    }

    #[test]
    fn keeps_the_place_of_each_string_argument() {
        let text = "{{ source('raw', \"orders\") }} {{ ref('pkg', 'm') }}";
        let found = calls(text);
        let arguments: Vec<Vec<&str>> =
            found.iter().map(|call| call.arguments.iter().map(|a| &text[a.clone()]).collect()).collect();
        assert_eq!(arguments, [vec!["raw", "orders"], vec![], vec!["pkg", "m"], vec![]]);
    }

    #[test]
    fn finds_the_calls_of_yaml_without_delimiters_nor_comments() {
        let text =
            "      - relationships:\n          to: source('raw', 'customers')  # not ref('x')\n          field: id\n";
        let found = yaml_calls(text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].target, Target::Source { source: "raw".into(), table: "customers".into() });
        assert_eq!(&text[found[0].arguments[1].clone()], "customers");
    }

    #[test]
    fn outline_orders_symbols_by_start_then_outermost_first() {
        let text = "{% snapshot s %}with a as (select 1) select * from a{% endsnapshot %}";
        assert_eq!(names(&outline(text, true)), [(Kind::Snapshot, "s"), (Kind::Cte, "a")]);
        assert_eq!(names(&outline(text, false)), [(Kind::Snapshot, "s")]);
    }
}
