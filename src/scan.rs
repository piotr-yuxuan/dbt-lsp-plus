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
    /// Jinja's control blocks, which only the outline names.
    If,
    For,
    Call,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Symbol {
    pub kind: Kind,
    pub name: String,
    pub range: Range<usize>,
    pub name_range: Range<usize>,
    /// What the symbol holds: a block's text between its tags, a CTE's
    /// inside its parentheses.
    pub inner: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Ref {
        package: Option<String>,
        name: String,
        /// The version that `v=` or `version=` names, as written; empty
        /// when it is no literal, such as `v=var('x')`.
        version: Option<String>,
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

/// Each span of TEXT that one of OPENERS opens, (opener, closer), in order,
/// with the index of its opener.  A span runs to its closer, or to the end
/// of the text; an opener inside a span does not count.
fn spans(text: &str, openers: &[(&str, &str)]) -> Vec<(usize, Range<usize>)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let Some(which) = openers.iter().position(|(open, _)| bytes[at..].starts_with(open.as_bytes())) else {
            at += 1;
            continue;
        };
        let (open, close) = openers[which];
        let from = at + open.len();
        let end = text[from..].find(close).map_or(text.len(), |found| from + found + close.len());
        found.push((which, at..end));
        at = end;
    }
    found
}

/// Blanks each span that one of SPANS opens: (opener, closer, blank).  A
/// span with blank false, such as a string, is skipped whole, so that an
/// opener inside it does not count.  The text keeps its length, and its
/// offsets their meaning.
fn mask(text: &str, spans: &[(&str, &str, bool)]) -> String {
    let openers: Vec<(&str, &str)> = spans.iter().map(|(open, close, _)| (*open, *close)).collect();
    let mut out = text.as_bytes().to_vec();
    for (which, range) in self::spans(text, &openers) {
        if spans[which].2 {
            blank(&mut out, range);
        }
    }
    // Every span starts and ends at an ASCII delimiter, so whole characters are blanked.
    String::from_utf8(out).expect("masking blanks whole characters")
}

/// TEXT without its Jinja comments, the only comments Jinja honours: a tag
/// inside a SQL comment is rendered all the same.
fn mask_jinja_comments(text: &str) -> String {
    mask(text, &[("{#", "#}", true)])
}

// Either trim marker, as Jinja's lexer takes them.
static RAW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A\{%[-+]?\s*raw\s*[-+]?%\}\z").unwrap());
static END_RAW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{%[-+]?\s*endraw\s*[-+]?%\}").unwrap());

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

/// TEXT without its Jinja comments and tags: the SQL as Jinja leaves it,
/// give or take what the tags render.
fn without_jinja(text: &str) -> String {
    let masked = mask_jinja_comments(text);
    let mut out = masked.clone().into_bytes();
    jinja_tags(&masked).into_iter().for_each(|tag| blank(&mut out, tag));
    String::from_utf8(out).expect("masking blanks whole characters")
}

/// TEXT as SQL's parentheses see it: without Jinja comments and tags, SQL
/// comments and SQL strings.  Double-quoted identifiers are kept.
fn mask_code(text: &str) -> String {
    mask(&without_jinja(text), &[("--", "\n", true), ("/*", "*/", true), ("'", "'", true), ("\"", "\"", false)])
}

/// The Jinja tags of TEXT, `{{ }}` and `{% %}`, outside Jinja comments.
pub fn tags(text: &str) -> Vec<Range<usize>> {
    jinja_tags(&mask_jinja_comments(text))
}

/// The string literals inside the Jinja tags of TEXT that a macro may render
/// as SQL, as `is_positive('orders.amount')` does: all but the arguments of
/// `ref` and `source`, which name models and sources.
pub fn jinja_strings(text: &str) -> Vec<Range<usize>> {
    let named: Vec<Range<usize>> = calls(text)
        .into_iter()
        .filter(|call| matches!(call.target, Target::Ref { .. } | Target::Source { .. }))
        .flat_map(|call| call.arguments)
        .collect();
    let mut found = Vec::new();
    for tag in tags(text) {
        let strings = spans(&text[tag.clone()], &[("'", "'"), ("\"", "\"")]).into_iter();
        found.extend(strings.map(|(_, string)| tag.start + string.start..tag.start + string.end));
    }
    found.retain(|string| !named.iter().any(|argument| string.start < argument.start && argument.end < string.end));
    found
}

/// The comments of TEXT that may span lines: Jinja's `{# #}`, and SQL's
/// `/* */` outside Jinja and strings.
pub fn comments(text: &str) -> Vec<Range<usize>> {
    let jinja = spans(text, &[("{#", "#}")]).into_iter().map(|(_, range)| range);
    let sql_openers = [("'", "'"), ("\"", "\""), ("--", "\n"), ("/*", "*/")];
    let sql = spans(&without_jinja(text), &sql_openers).into_iter().filter(|(which, _)| *which == 3);
    let mut found: Vec<Range<usize>> = jinja.chain(sql.map(|(_, range)| range)).collect();
    found.sort_by_key(|range| range.start);
    found
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
                symbol.inner.end = range.start;
                done.push(symbol);
            }
            continue;
        }
        let rest = tag.get(3).unwrap();
        let block_set = kind != Kind::Set || !rest.as_str().contains('=');
        if let (true, Some(name)) = (block_set, BLOCK_NAME.captures(rest.as_str())) {
            let name = name.get(1).unwrap();
            let name_range = range.start + rest.start() + name.start()..range.start + rest.start() + name.end();
            let (range, inner) = (range.start..text.len(), range.end..text.len());
            open.push(Symbol { kind, name: name.as_str().to_owned(), range, name_range, inner });
        }
    }
    done.extend(open);
    done.sort_by_key(|symbol| symbol.range.start);
    done
}

// Either trim marker, `-` or `+`, on either side.
static TAG_KEYWORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)\A\{%[-+]?\s*(\w+)(.*?)[-+]?%\}\z").unwrap());

/// The keywords that open a Jinja block, closed by `end` and the keyword.
const BLOCK_OPENERS: [&str; 12] =
    ["if", "for", "macro", "call", "filter", "set", "raw", "test", "snapshot", "materialization", "docs", "block"];

/// A Jinja block of a text, by its tags.
#[derive(Clone, Debug, PartialEq)]
pub struct TagBlock {
    /// What opens it: `if`, `macro` and the others of BLOCK_OPENERS.
    pub keyword: String,
    /// Its opening tag, its `elif` and `else` tags, then its closing tag
    /// when it is CLOSED.
    pub tags: Vec<Range<usize>>,
    pub closed: bool,
}

/// The Jinja blocks of TEXT: those closed, in the order they close, then
/// those left open.  The inline `set x = ...` opens none.
pub fn block_tags(text: &str) -> Vec<TagBlock> {
    let mut open: Vec<(String, Vec<Range<usize>>)> = Vec::new();
    let mut done = Vec::new();
    for tag in tags(text) {
        let Some(found) = TAG_KEYWORD.captures(&text[tag.clone()]) else { continue };
        let (keyword, rest) = (&found[1], &found[2]);
        let closed = keyword.strip_prefix("end").and_then(|kind| open.iter().rposition(|(opener, _)| opener == kind));
        match (keyword, closed) {
            ("elif" | "else", _) => open.last_mut().into_iter().for_each(|(_, tags)| tags.push(tag.clone())),
            (_, Some(at)) => {
                let (keyword, mut tags) = open.remove(at);
                tags.push(tag);
                done.push(TagBlock { keyword, tags, closed: true });
            }
            (_, None) if BLOCK_OPENERS.contains(&keyword) && !(keyword == "set" && rest.contains('=')) => {
                open.push((keyword.to_owned(), vec![tag]));
            }
            _ => {}
        }
    }
    done.extend(open.into_iter().map(|(keyword, tags)| TagBlock { keyword, tags, closed: false }));
    done
}

/// The `if`, `for` and `call` blocks of TEXT, as symbols: each named by its
/// opening tag's statement, as written but for its trim markers and with
/// each run of spaces, tabs and line ends one space, as dbt-mode names them
/// (`if is_incremental()`); from its opening tag, its name's place, to the
/// end of its closing tag, else of the text.
pub fn control_blocks(text: &str) -> Vec<Symbol> {
    let kinds = [("if", Kind::If), ("for", Kind::For), ("call", Kind::Call)];
    let symbol = |block: TagBlock| {
        let kind = kinds.iter().find(|(keyword, _)| *keyword == block.keyword)?.1;
        let opening = block.tags[0].clone();
        let statement = text[opening.clone()].trim_start_matches("{%").trim_end_matches("%}");
        let words = statement.trim_matches(['-', '+']).split([' ', '\t', '\n', '\r']);
        let name = words.filter(|word| !word.is_empty()).collect::<Vec<_>>().join(" ");
        let closing = block.tags.last().filter(|_| block.closed);
        let (end, inner_end) = closing.map_or((text.len(), text.len()), |closing| (closing.end, closing.start));
        Some(Symbol {
            kind,
            name,
            range: opening.start..end,
            name_range: opening.clone(),
            inner: opening.end..inner_end,
        })
    };
    block_tags(text).into_iter().filter_map(symbol).collect()
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
            inner: open + 1..close.unwrap_or(text.len()),
        }
    };
    CTE.captures_iter(&code).map(symbol).collect()
}

/// The places of NAME in TEXT as a whole word, whose neighbours are no
/// letter, digit, `_` or `$`; without regard to case unless NAME is quoted.
pub fn whole_words(text: &str, name: &str) -> Vec<Range<usize>> {
    let escaped = regex::escape(name);
    let pattern = if name.starts_with('"') { escaped } else { format!("(?i){escaped}") };
    let Ok(word) = Regex::new(&pattern) else { return Vec::new() };
    let identifier = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let found = word.find_iter(text).map(|found| found.range());
    found
        .filter(|range| !text[..range.start].ends_with(identifier) && !text[range.end..].starts_with(identifier))
        .collect()
}

/// Each CTE of TEXT with the places that use it: whole words outside Jinja,
/// comments and strings, within the innermost block that defines it (an
/// adapter's macro beside another's), not after a `.` and not another CTE's
/// definition.  A column or an alias spelt like a CTE counts.
pub fn cte_uses(text: &str) -> Vec<(Symbol, Vec<Range<usize>>)> {
    let code = mask_code(text);
    let (ctes, blocks) = (ctes(text), blocks(text));
    let definitions: Vec<Range<usize>> = ctes.iter().map(|cte| cte.name_range.clone()).collect();
    let uses = |cte: &Symbol| -> Vec<Range<usize>> {
        let holds = |range: &&Range<usize>| range.start <= cte.name_range.start && cte.name_range.end <= range.end;
        let scope = blocks.iter().map(|block| &block.range).filter(holds).min_by_key(|range| range.len());
        let scope = scope.cloned().unwrap_or(0..text.len());
        let found = whole_words(&code, &cte.name).into_iter().filter(|range| scope.contains(&range.start));
        found.filter(|range| !definitions.contains(range) && !code[..range.start].ends_with('.')).collect()
    };
    ctes.into_iter()
        .map(|cte| {
            let found = uses(&cte);
            (cte, found)
        })
        .collect()
}

/// The blocks of TEXT, its CTEs when it is SQL, and its control blocks
/// when CONTROL, ordered by start, the longest first.
pub fn outline(text: &str, sql: bool, control: bool) -> Vec<Symbol> {
    let mut symbols = blocks(text);
    if sql {
        symbols.extend(ctes(text));
    }
    if control {
        // A block left open ends with the innermost symbol that holds its
        // opening tag, rather than cross it: Jinja refuses that symbol's end
        // while the block is open, and dbt-mode draws it within.
        let within = |block: Symbol| {
            let crossed =
                |symbol: &&Symbol| symbol.range.contains(&block.range.start) && symbol.range.end < block.range.end;
            let end = symbols.iter().filter(crossed).map(|symbol| symbol.range.end).min().unwrap_or(block.range.end);
            Symbol { range: block.range.start..end, inner: block.inner.start..block.inner.end.min(end), ..block }
        };
        let control: Vec<Symbol> = control_blocks(text).into_iter().map(within).collect();
        symbols.extend(control);
    }
    symbols.sort_by_key(|symbol| (symbol.range.start, std::cmp::Reverse(symbol.range.end)));
    symbols
}

// Its arguments are literals, then `)` or a keyword argument: `'orders' ~ var('sfx')` names another
// model, and `'pkg', 'm' ~ x` names no model `pkg`.  The call, group 1, ends at its last literal; a
// keyword, group 4, has its value in group 5 when that is a literal.
static REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(\bref\s*\(\s*['"]([^'"]*)['"](?:\s*,\s*['"]([^'"]*)['"])?)\s*(?:\)|,\s*(\w+)\s*=\s*(?:['"]?([\w.\-]+)['"]?\s*[,)])?)"#,
    )
    .unwrap()
});
// Its arguments are two literals: `'events' ~ var('suffix')` names another table.
static SOURCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bsource\s*\(\s*['"]([^'"]*)['"]\s*,\s*['"]([^'"]*)['"]\s*\)"#).unwrap());
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
        let arguments = [call.get(2), call.get(3)].into_iter().flatten().map(shift).collect();
        let (package, name) = match call.get(3) {
            Some(name) => (Some(call[2].to_owned()), name.as_str().to_owned()),
            None => (None, call[2].to_owned()),
        };
        let version = matches!(call.get(4).map(|keyword| keyword.as_str()), Some("v" | "version"))
            .then(|| call.get(5).map_or_else(String::new, |version| version.as_str().to_owned()));
        let target = Target::Ref { package, name, version };
        found.push(Call { target, range: shift(call.get(1).unwrap()), arguments });
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
    let masked = without_yaml_comments(text);
    let mut found = refs_and_sources(&masked, 0);
    found.sort_by_key(|call| call.range.start);
    found
}

/// TEXT, YAML, its comments outside quotes blanked, offsets kept: what dbt
/// renders of it.
pub fn without_yaml_comments(text: &str) -> String {
    mask(text, &[("'", "'", false), ("\"", "\"", false), ("#", "\n", true)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ref_with_an_expression_for_argument_names_no_model() {
        let text =
            "{{ ref('orders' ~ var('sfx')) }} {{ ref('dim', v=2) }} {{ ref('pkg', 'm') }} {{ ref('pkg', 'n' ~ x) }}";
        let refs: Vec<Target> =
            calls(text).into_iter().map(|call| call.target).filter(|t| matches!(t, Target::Ref { .. })).collect();
        let named = |package: Option<&str>, name: &str, version: Option<&str>| Target::Ref {
            package: package.map(str::to_owned),
            name: name.into(),
            version: version.map(str::to_owned),
        };
        assert_eq!(refs, [named(None, "dim", Some("2")), named(Some("pkg"), "m", None)]);
    }

    #[test]
    fn a_ref_names_its_version_as_written() {
        let text = "{{ ref('a', v=2) }} {{ ref('b', version = '1.5') }} {{ ref('p', 'c', v=\"3\") }} \
                    {{ ref('d', v=var('x')) }} {{ ref('e') }}";
        let versions: Vec<Option<String>> = calls(text)
            .into_iter()
            .filter_map(|call| match call.target {
                Target::Ref { version, .. } => Some(version),
                _ => None,
            })
            .collect();
        let expected = [Some("2"), Some("1.5"), Some("3"), Some(""), None].map(|version| version.map(str::to_owned));
        assert_eq!(versions, expected);
    }

    #[test]
    fn a_source_with_an_expression_for_argument_names_no_table() {
        let text = "{{ source('raw', 'events' ~ var('suffix')) }} {{ source( 'raw' , \"orders\" ) }}";
        let targets: Vec<Target> = calls(text).into_iter().map(|call| call.target).collect();
        let orders = Target::Source { source: "raw".into(), table: "orders".into() };
        assert!(targets.contains(&orders), "{targets:?}");
        assert!(!targets.iter().any(|t| matches!(t, Target::Source { table, .. } if table == "events")), "{targets:?}");
    }

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
            names(&outline(text, true, false)),
            [(Kind::Macro, "métrique"), (Kind::Cte, "résultat"), (Kind::Cte, "総計")]
        );
    }

    #[test]
    fn knows_what_a_block_or_a_cte_holds() {
        let text = "{% macro m() %}body{% endmacro %} with c (x) as (select (1)) select 1";
        let found = outline(text, true, false);
        assert_eq!(&text[found[0].inner.clone()], "body");
        assert_eq!(&text[found[1].inner.clone()], "select (1)");
    }

    #[test]
    fn finds_the_comments_that_may_span_lines() {
        let text = "{# a\n b #} select '/* no */' {{ '/* no */' }} /* yes\n */ -- /* no\n";
        let found: Vec<&str> = comments(text).into_iter().map(|range| &text[range]).collect();
        assert_eq!(found, ["{# a\n b #}", "/* yes\n */"]);
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
                Target::Ref { package: None, name: "live".into(), version: None },
                named(None, "ref"),
                Target::Ref { package: Some("pkg".into()), name: "b".into(), version: None },
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
        assert_eq!(names(&outline(text, true, false)), [(Kind::Snapshot, "s"), (Kind::Cte, "a")]);
        assert_eq!(names(&outline(text, false, false)), [(Kind::Snapshot, "s")]);
    }

    /// As agreed with dbt-mode for its breadcrumbs (plan 027).
    #[test]
    fn names_the_control_blocks_by_their_statement_as_written() {
        let text = "{%- if is_incremental() -%}a{% elif x %}b{% else %}c{%- endif %}\n\
                    {% for column in\n     columns %}{{ column }}{% endfor %}\n\
                    {%+ call(user) statement('main', fetch_result=True) %}d{% endcall %}\n\
                    {% set x = 1 %}{% if open %}e{% else %}f";
        let found = control_blocks(text);
        let expected = [
            (Kind::If, "if is_incremental()"),
            (Kind::For, "for column in columns"),
            (Kind::Call, "call(user) statement('main', fetch_result=True)"),
            (Kind::If, "if open"),
        ];
        assert_eq!(names(&found), expected, "elif, else and the closing tags are no symbols");
        let at = |part: &str| text.find(part).unwrap();
        assert_eq!(found[0].range, 0..at("\n"), "to the end of its closing tag");
        assert_eq!(found[0].name_range, 0..at("a{%"), "its opening tag");
        assert_eq!(found[3].range, at("{% if open")..text.len(), "left open, to the end, past its else");
        // Within a raw block, a tag is text, whichever trim marker opens it.
        assert_eq!(control_blocks("{%+ raw %}{% if x %}{% endraw +%}\nselect 1\n"), []);
    }

    #[test]
    fn a_control_block_left_open_ends_with_what_holds_it() {
        let text = "{% macro m() %}\n{% if a %}\nwith c as (select 1)\nselect * from c\n{% endmacro %}\n";
        let found = outline(text, true, true);
        assert_eq!(names(&found), [(Kind::Macro, "m"), (Kind::If, "if a"), (Kind::Cte, "c")]);
        assert_eq!(found[1].range.end, found[0].range.end, "within the macro");
        let cte = "with src as (\n  select * from t\n  {% if is_incremental() %}\n  where x > 1\n), f as (select 1)\n";
        let found = outline(cte, true, true);
        assert_eq!(found[1].range.end, found[0].range.end, "within the CTE: {found:?}");
    }
}
