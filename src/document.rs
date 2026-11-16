// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! What one document alone answers, which dbt lsp 2.0.9 does not: folding
//! ranges, highlights, selection ranges and the rename of a CTE.  Every
//! function here is pure.

use crate::scan::{self, Symbol};
use crate::text::{Encoding, LineIndex};
use regex::Regex;
use serde_json::{Value, json};
use std::ops::Range;
use std::sync::LazyLock;

/// The folding ranges of TEXT, by line: its Jinja blocks, its multi-line
/// Jinja tags and, when it is SQL, its CTEs (`region`); its multi-line
/// comments, Jinja's always and SQL's when it is SQL (`comment`).  A closer
/// alone at the start of its line stays visible: the range ends on the line
/// before.  The longest of the ranges that start on one line comes first,
/// since some clients keep only the first.
pub fn folding_ranges(text: &str, sql: bool) -> Value {
    let index = LineIndex::new(text);
    let fold = |start: usize, closer: usize, kind: &'static str| {
        let (first, mut last) = (index.line(start), index.line(closer));
        if text[index.line_start(last)..closer].trim().is_empty() {
            last = last.saturating_sub(1);
        }
        (last > first).then_some((first, last, kind))
    };
    // Where the closer of SPAN starts, with its whitespace control; a span
    // left open runs to the end of the text, with no closer.
    let closer = |span: &Range<usize>| {
        let written = &text[span.clone()];
        let closed = written.len() >= 4 && ["}}", "%}", "#}", "*/"].iter().any(|closer| written.ends_with(closer));
        let end = if closed { span.end - 2 } else { span.end };
        if closed && text[..end].ends_with(['-', '+']) { end - 1 } else { end }
    };
    let symbols =
        scan::outline(text, sql, false).into_iter().map(|symbol| fold(symbol.range.start, symbol.inner.end, "region"));
    let closed = scan::block_tags(text).into_iter().filter(|block| block.closed);
    let blocks = closed.map(|block| fold(block.tags[0].start, block.tags[block.tags.len() - 1].start, "region"));
    let tags = scan::tags(text).into_iter().map(|tag| fold(tag.start, closer(&tag), "region"));
    let comments = scan::comments(text).into_iter().filter(|comment| sql || text[comment.clone()].starts_with("{#"));
    let comments = comments.map(|comment| fold(comment.start, closer(&comment), "comment"));
    let mut ranges: Vec<(usize, usize, &str)> = symbols.chain(blocks).chain(tags).chain(comments).flatten().collect();
    ranges.sort_by_key(|&(first, last, kind)| (first, std::cmp::Reverse(last), kind));
    ranges.dedup_by_key(|(first, last, _)| (*first, *last));
    let ranges =
        ranges.into_iter().map(|(first, last, kind)| json!({"startLine": first, "endLine": last, "kind": kind}));
    Value::Array(ranges.collect())
}

/// The CTE of TEXT that OFFSET names, by its definition or one of its uses,
/// with its uses.
pub fn cte_at(text: &str, offset: usize) -> Option<(Symbol, Vec<Range<usize>>)> {
    let inside = |range: &Range<usize>| range.start <= offset && offset <= range.end;
    scan::cte_uses(text).into_iter().find(|(cte, uses)| inside(&cte.name_range) || uses.iter().any(inside))
}

/// The highlights at OFFSET of TEXT: on a CTE of SQL, its definition (write)
/// and its uses (read); on a tag of a Jinja block, every tag of that block.
pub fn highlights(text: &str, sql: bool, offset: usize, encoding: Encoding) -> Value {
    let index = LineIndex::new(text);
    let highlight =
        |range: &Range<usize>, kind: u8| json!({"range": index.range(range.start, range.end, encoding), "kind": kind});
    if let Some((cte, uses)) = cte_at(text, offset).filter(|_| sql) {
        let definition = std::iter::once(highlight(&cte.name_range, 3));
        return Value::Array(definition.chain(uses.iter().map(|range| highlight(range, 2))).collect());
    }
    let inside = |range: &Range<usize>| range.start <= offset && offset <= range.end;
    let block = scan::block_tags(text).into_iter().find(|block| block.closed && block.tags.iter().any(inside));
    block.map_or(Value::Null, |block| Value::Array(block.tags.iter().map(|tag| highlight(tag, 1)).collect()))
}

static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}_$]+").unwrap());

/// The selection range at each of OFFSETS of TEXT: from the innermost, the
/// word, the Jinja tag, a CTE's or block's content, the CTE or block, and
/// the whole text, each range containing the one before.
pub fn selection_ranges(text: &str, sql: bool, offsets: &[usize], encoding: Encoding) -> Value {
    let index = LineIndex::new(text);
    let mut candidates = scan::tags(text);
    for symbol in scan::outline(text, sql, false) {
        candidates.extend([symbol.inner, symbol.range]);
    }
    for block in scan::block_tags(text).into_iter().filter(|block| block.closed) {
        let (opening, closing) = (&block.tags[0], &block.tags[block.tags.len() - 1]);
        candidates.extend([opening.end..closing.start, opening.start..closing.end]);
    }
    candidates.push(0..text.len());
    let chain = |&offset: &usize| {
        let around = |range: &&Range<usize>| range.start <= offset && offset <= range.end;
        let word = WORD.find_iter(text).map(|word| word.range()).find(|word| around(&word));
        let mut ranges: Vec<Range<usize>> = word.into_iter().chain(candidates.iter().filter(around).cloned()).collect();
        ranges.sort_by_key(|range| (range.len(), range.start));
        let mut nested: Vec<Range<usize>> = Vec::new();
        for range in ranges {
            if nested.last().is_none_or(|inner| range.start <= inner.start && inner.end <= range.end && range != *inner)
            {
                nested.push(range);
            }
        }
        nested.iter().rev().fold(None, |parent: Option<Value>, range| {
            let mut selection = json!({"range": index.range(range.start, range.end, encoding)});
            if let Some(parent) = parent {
                selection["parent"] = parent;
            }
            Some(selection)
        })
    };
    Value::Array(offsets.iter().filter_map(chain).collect())
}

static IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A[\p{L}_][\p{L}\p{N}_$]*\z").unwrap());

/// The `WorkspaceEdit` that renames to NEW the CTE that OFFSET names in TEXT,
/// the document at URI, or the reason it cannot; `None` when OFFSET names no
/// CTE.
pub fn rename_cte(
    text: &str,
    uri: &str,
    offset: usize,
    new: &str,
    encoding: Encoding,
) -> Option<Result<Value, String>> {
    let (cte, uses) = cte_at(text, offset)?;
    let mut rendered = scan::jinja_strings(text).into_iter();
    if let Some(string) = rendered.find(|string| !scan::whole_words(&text[string.clone()], &cte.name).is_empty()) {
        let string = &text[string];
        return Some(Err(format!(
            "the Jinja string {string} names {}, which a macro may render: rename it by hand",
            cte.name
        )));
    }
    if !IDENTIFIER.is_match(new) {
        return Some(Err(format!("{new} is not a plain identifier")));
    }
    if scan::ctes(text).iter().any(|other| other.name_range != cte.name_range && other.name.eq_ignore_ascii_case(new)) {
        return Some(Err(format!("another CTE is named {new}")));
    }
    let index = LineIndex::new(text);
    let edits = std::iter::once(&cte.name_range).chain(&uses);
    let edits = edits.map(|range| json!({"range": index.range(range.start, range.end, encoding), "newText": new}));
    Some(Ok(json!({"changes": {uri: edits.collect::<Vec<_>>()}})))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDERS: &str = include_str!("../tests/fixtures/shop/models/marts/orders.sql");

    fn line_of(text: &str, needle: &str) -> usize {
        text[..text.find(needle).unwrap()].matches('\n').count()
    }

    #[test]
    fn folds_comments_and_ctes_keeping_their_closers_visible() {
        let found = folding_ranges(ORDERS, true);
        let found: Vec<(u64, u64, &str)> = found
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["startLine"].as_u64().unwrap(), r["endLine"].as_u64().unwrap(), r["kind"].as_str().unwrap()))
            .collect();
        assert_eq!(found, [(0, 1, "comment"), (3, 4, "region"), (7, 8, "region"), (11, 18, "region")]);
    }

    #[test]
    fn folds_blocks_and_multi_line_tags() {
        // The block's end tag stands alone, so line 4 stays visible; the tag's `}}` follows a `)`, so its line folds.
        let text = "{% macro m() %}\n  {{ config(\n    a=1\n  ) }}\n{% endmacro %}\nselect 1";
        let found = folding_ranges(text, true);
        assert_eq!(
            found,
            json!([{"startLine": 0, "endLine": 3, "kind": "region"}, {"startLine": 1, "endLine": 3, "kind": "region"}])
        );
    }

    #[test]
    fn highlights_a_cte_and_its_uses() {
        let offset = ORDERS.find("from orders").unwrap() + 6;
        let found = highlights(ORDERS, true, offset, Encoding::Utf16);
        let lines: Vec<(u64, u64)> = found
            .as_array()
            .unwrap()
            .iter()
            .map(|h| (h["range"]["start"]["line"].as_u64().unwrap(), h["kind"].as_u64().unwrap()))
            .collect();
        let at = |needle: &str| line_of(ORDERS, needle) as u64;
        assert_eq!(
            lines,
            [(at("with orders"), 3), (at("orders.order_id"), 2), (at("orders.amount,"), 2), (at("from orders"), 2)]
        );
    }

    #[test]
    fn highlights_every_tag_of_a_jinja_block() {
        let text = "{% if a %}x{% elif b %}y{% else %}{% set v = 1 %}{% for i in l %}z{% endfor %}{% endif %}";
        let found = highlights(text, true, text.find("endif").unwrap(), Encoding::Utf16);
        let tags: Vec<&str> = found
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                let start = h["range"]["start"]["character"].as_u64().unwrap() as usize;
                let end = h["range"]["end"]["character"].as_u64().unwrap() as usize;
                &text[start..end]
            })
            .collect();
        assert_eq!(tags, ["{% if a %}", "{% elif b %}", "{% else %}", "{% endif %}"]);
        assert_eq!(highlights(text, true, text.find("set").unwrap(), Encoding::Utf16), Value::Null);
    }

    #[test]
    fn selection_ranges_nest_up_to_the_whole_text() {
        let offset = ORDERS.find("stg_customers").unwrap() + 2;
        let found = selection_ranges(ORDERS, true, &[offset], Encoding::Utf16);
        let mut ranges = Vec::new();
        let mut selection = &found[0];
        while selection.is_object() {
            ranges.push(selection["range"].clone());
            selection = &selection["parent"];
        }
        let texts: Vec<String> = ranges
            .iter()
            .map(|r| {
                let index = LineIndex::new(ORDERS);
                let start = index.offset_of(&r["start"], Encoding::Utf16).unwrap();
                let end = index.offset_of(&r["end"], Encoding::Utf16).unwrap();
                ORDERS[start..end].to_owned()
            })
            .collect();
        assert_eq!(texts[0], "stg_customers");
        assert_eq!(texts[1], "{{ ref('stg_customers') }}");
        assert!(texts[2].trim().starts_with("select * from {{ ref('stg_customers') }}"));
        assert!(texts[3].starts_with("customers as (") && texts[3].ends_with(')'));
        assert_eq!(texts.last().unwrap(), ORDERS);
    }

    /// TEXT once the edits of EDIT for URI are applied.
    fn applied(text: &str, edit: &Value, uri: &str) -> String {
        let mut text = text.to_owned();
        let mut edits: Vec<&Value> = edit["changes"][uri].as_array().unwrap().iter().collect();
        edits.sort_by_key(|e| {
            std::cmp::Reverse((e["range"]["start"]["line"].as_u64(), e["range"]["start"]["character"].as_u64()))
        });
        for e in edits {
            crate::text::apply_change(&mut text, &json!({"range": e["range"], "text": e["newText"]}), Encoding::Utf16);
        }
        text
    }

    #[test]
    fn renames_a_cte_and_refuses_a_clash_or_a_bad_name() {
        let uri = "file:///o.sql";
        let offset = ORDERS.find("left join customers").unwrap() + 12;
        let edit = rename_cte(ORDERS, uri, offset, "c", Encoding::Utf16).unwrap().unwrap();
        let text = applied(ORDERS, &edit, uri);
        assert!(text.contains("\nc as (") && text.contains("c.first_name") && text.contains("left join c using"));
        assert!(text.contains("{{ ref('stg_customers') }}"), "{text}");
        let clash = rename_cte(ORDERS, uri, offset, "Final", Encoding::Utf16).unwrap();
        assert!(clash.unwrap_err().contains("another CTE"));
        for bad in ["a b", "1x", "x-y", ""] {
            assert!(rename_cte(ORDERS, uri, offset, bad, Encoding::Utf16).unwrap().is_err(), "{bad:?}");
        }
        assert!(rename_cte(ORDERS, uri, offset, "Customers", Encoding::Utf16).unwrap().is_ok(), "a case of its own");
        assert!(rename_cte(ORDERS, uri, 0, "x", Encoding::Utf16).is_none());
    }

    #[test]
    fn refuses_to_rename_a_cte_that_a_jinja_string_names() {
        // is_positive renders its argument, 'orders.amount', as SQL.
        let offset = ORDERS.find("from orders").unwrap() + 6;
        let refused = rename_cte(ORDERS, "file:///o.sql", offset, "o", Encoding::Utf16).unwrap();
        assert!(refused.unwrap_err().contains("'orders.amount'"));
        let star = "with base as (select 1) select {{ star(relation_alias='base') }} from base";
        assert!(rename_cte(star, "file:///s.sql", 6, "b", Encoding::Utf16).unwrap().is_err());
        // A function's name, or the model or source of a ref or source, renders no CTE.
        for text in [
            "with source as (select * from {{ source('raw', 'source') }}) select * from source",
            "with orders as (select * from {{ ref('orders') }}) select * from orders",
        ] {
            let renamed = rename_cte(text, "file:///t.sql", 6, "base", Encoding::Utf16).unwrap();
            assert!(renamed.is_ok(), "{text}: {renamed:?}");
        }
    }

    #[test]
    fn a_cte_s_uses_are_its_words_in_code_and_in_its_block() {
        let text =
            "with Orders as (select 1) -- orders\nselect 'orders', analytics.orders, orders$x, orders from ORDERS";
        let edit =
            rename_cte(text, "file:///t.sql", text.find("Orders").unwrap(), "o", Encoding::Utf16).unwrap().unwrap();
        let text = applied(text, &edit, "file:///t.sql");
        assert_eq!(text, "with o as (select 1) -- orders\nselect 'orders', analytics.orders, orders$x, o from o");
        let dollar = "with a$ as (select 1) select * from a$, a$b";
        let (_, uses) = cte_at(dollar, 6).unwrap();
        let used = dollar.rfind("a$,").unwrap();
        assert_eq!((uses.len(), uses[0].clone()), (1, used..used + 2));
        // Two adapters' macros define the same CTE: each keeps its own.
        let macros = "{% macro default__d() %}with rows as (select 1) select * from rows{% endmacro %}\n\
                      {% macro duckdb__d() %}with rows as (select 2) select * from rows{% endmacro %}";
        let second = macros.rfind("from rows").unwrap() + 6;
        let found = highlights(macros, true, second, Encoding::Utf16);
        let starts: Vec<u64> =
            found.as_array().unwrap().iter().map(|h| h["range"]["start"]["character"].as_u64().unwrap()).collect();
        let line = &macros[macros.find('\n').unwrap() + 1..];
        assert_eq!(starts, [line.find("rows").unwrap() as u64, line.rfind("rows").unwrap() as u64]);
    }

    /// The folds of TEXT as (start line, end line).
    fn folds(text: &str, sql: bool) -> Vec<(u64, u64)> {
        let found = folding_ranges(text, sql);
        found
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["startLine"].as_u64().unwrap(), r["endLine"].as_u64().unwrap()))
            .collect()
    }

    #[test]
    fn folds_every_jinja_block_and_keeps_a_lone_closer_visible() {
        let incremental = "select *\nfrom t\n{% if is_incremental() %}\nwhere a > 1\n  and b > 2\n{% endif %}\n";
        assert_eq!(folds(incremental, true), [(2, 4)]);
        let loop_ = "select\n{% for c in cols %}\n  {{ c }},\n  {{ c }}_x,\n{%- endfor %}\n1";
        assert_eq!(folds(loop_, true), [(1, 3)]);
        // A closer with whitespace control alone on its line stays visible too.
        assert_eq!(folds("{{-\n  config(\n    a=1\n  )\n-}}\nselect 1", true), [(0, 3)]);
        assert_eq!(folds("{#-\n a\n b\n-#}\nselect 1", true), [(0, 2)]);
    }

    #[test]
    fn folds_a_span_left_open_to_the_end_without_failing() {
        for text in ["/* draft\nselect 1 -- filtré\n", "{# new\nselect 1 -- 注\n", "{{ config(\n  a='注"] {
            let found = folds(text, true);
            assert_eq!(found.first().map(|fold| fold.0), Some(0), "{text:?}: {found:?}");
        }
    }

    /// Only the outline takes a block left open, to the end of the text.
    #[test]
    fn a_block_left_open_is_no_fold_highlight_nor_selection() {
        let text = "{% if a %}\nx\n{% else %}\ny\n";
        assert_eq!(folds(text, true), []);
        assert_eq!(highlights(text, true, 3, Encoding::Utf16), Value::Null);
        let chain = selection_ranges(text, true, &[11], Encoding::Utf16);
        assert_eq!(chain[0]["parent"]["range"]["start"], json!({"line": 0, "character": 0}), "the word, then the text");
        assert_eq!(chain[0]["parent"].get("parent"), None);
    }

    #[test]
    fn folds_sql_comments_in_sql_only_and_the_longest_range_first() {
        assert_eq!(folds("a: 1\n# under models/* every file\nb:\n  - 2\n", false), []);
        let test = "{% test t(\n    model,\n    column\n) %}\nselect 1\nfrom {{ model }}\n{% endtest %}\n";
        assert_eq!(folds(test, false), [(0, 5), (0, 3)]);
    }

    #[test]
    fn selection_ranges_hold_blocks_each_position_and_no_range_twice() {
        let text = "{% if a %}\nselect x\n{% endif %}";
        let at = text.find('x').unwrap();
        let found = selection_ranges(text, true, &[at, 0], Encoding::Utf16);
        assert_eq!(found.as_array().unwrap().len(), 2, "one chain per position");
        let mut lengths = Vec::new();
        let mut selection = &found[0];
        while selection.is_object() {
            let (start, end) = (&selection["range"]["start"], &selection["range"]["end"]);
            let index = LineIndex::new(text);
            lengths.push(
                index.offset_of(end, Encoding::Utf16).unwrap() - index.offset_of(start, Encoding::Utf16).unwrap(),
            );
            selection = &selection["parent"];
        }
        // x, the block's content, then the block, which is the whole text: given once.
        assert_eq!(lengths, [1, "\nselect x\n".len(), text.len()]);
    }

    #[test]
    fn highlights_the_tags_of_the_block_asked_about() {
        let text = "{% macro m() %}{% if a %}{% set x = 1 %}{% if b %}{% endif %}{% elif c %}{% else %}{% endif %}{% endmacro %}";
        let tags = |offset: usize| -> Vec<String> {
            let found = highlights(text, true, offset, Encoding::Utf16);
            found
                .as_array()
                .unwrap()
                .iter()
                .map(|h| {
                    let start = h["range"]["start"]["character"].as_u64().unwrap() as usize;
                    let end = h["range"]["end"]["character"].as_u64().unwrap() as usize;
                    text[start..end].to_owned()
                })
                .collect()
        };
        assert_eq!(tags(text.find("elif").unwrap()), ["{% if a %}", "{% elif c %}", "{% else %}", "{% endif %}"]);
        assert_eq!(tags(text.find("if b").unwrap()), ["{% if b %}", "{% endif %}"]);
        assert_eq!(tags(text.find("endmacro").unwrap()), ["{% macro m() %}", "{% endmacro %}"]);
    }
}
