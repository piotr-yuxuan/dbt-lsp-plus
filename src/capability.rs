// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! What a session offers: one descriptor per method or command, who answers
//! it and how.  The capabilities announced, `experimental.dbtLspPlus`, the
//! commands listed and the methods that take a mode all come from
//! `CAPABILITIES`.

use crate::commands::{COMPILE, NODES, SHOW};
use crate::config::NODE_INFO;
use serde_json::{Value, json};
use std::time::Duration;

/// A request to which the proxy adds a part is answered with it alone if the
/// server has not answered within this long: three times what dbt lsp 2.0.9
/// took on the fixture during a compilation.
const MERGE_DEADLINE: Duration = Duration::from_millis(300);
/// A merged request to which the proxy has nothing to add is answered with
/// that nothing if the server has not answered within this long: above the
/// slowest answer of dbt lsp 2.0.9 seen, 1.9 s for the first hover on 2,000
/// models during its compilation.
const NOTHING_DEADLINE: Duration = Duration::from_secs(2);

/// Who can answer a method or a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// The proxy, from its scan and the parse's index.
    Plus,
    /// dbt lsp, by the method or by its own command.
    DbtLsp,
    /// dbt's CLI, run by the proxy under the cut.
    DbtCli,
}

/// How the answers of a method's providers combine in `merge`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Combine {
    /// One provider answers: for a method the proxy adds, a follower and
    /// nodeInfo, the proxy, whole; for a rename, the proxy at its kinds,
    /// else dbt lsp; for a command, the first of its providers present.
    First,
    /// The union of the locations, the proxy's first.
    Union,
    /// The proxy's part first, then dbt lsp's.
    Sections,
    /// dbt lsp's, then the proxy's.
    Append,
    /// Both, file by file.
    PerFile,
}

/// What the proxy offers of one LSP method or one command for clients.
#[derive(Debug)]
pub struct Capability {
    /// The LSP method, or the command.
    pub name: &'static str,
    /// The server capability that announces it.
    pub key: Option<&'static str>,
    /// Who can answer it, in the order they are asked.
    pub providers: &'static [Provider],
    /// dbt lsp's own command, for a command of the proxy's.
    pub server: Option<&'static str>,
    /// The method whose mode it takes, as its items come from it.
    pub leader: Option<&'static str>,
    /// Shipped `plus` unless the first server announces it, else `dbt`;
    /// when false, a method ships `merge`.
    pub added: bool,
    /// How the providers' answers combine.
    pub combine: Combine,
    /// Its nothing is `[]` rather than null.
    pub list: bool,
    /// The proxy's answer asks for the parse's index, in both modes: a
    /// stale index is parsed again, as for a hover.
    pub wants_index: bool,
    /// What the proxy answers itself, as `experimental.dbtLspPlus`
    /// announces it, version 2, since dbt lsp 2.0.9 answers with nothing
    /// there: each kind of target with the places where a request's
    /// position may be, `sql`, `yaml` or `python` by the file's extension.
    /// A kind listed at a place is the proxy's at every position of that
    /// kind there, with dbt lsp or without; in `merge` with a backend, dbt
    /// lsp is asked too, and its locations that match none of the proxy's
    /// follow them.  Empty for a method the proxy answers whole.
    pub kinds: &'static [(&'static str, &'static [&'static str])],
    /// Whether a request on a document of another project, which it names
    /// by `textDocument.uri`, gets `Elsewhere` (`Proxy::outside`): its
    /// answer there would read the session's project, its scan or its
    /// index, beyond the document's text.
    pub reads_project: bool,
}

/// What a row of `CAPABILITIES` does not say: an LSP method that both
/// answer, one at a time.
const DEFAULT: Capability = Capability {
    name: "",
    key: None,
    providers: &[Provider::Plus, Provider::DbtLsp],
    server: None,
    leader: None,
    added: false,
    combine: Combine::First,
    list: false,
    wants_index: false,
    kinds: &[],
    reads_project: false,
};

/// Every method and command that the proxy answers or takes part in, a
/// method's followers after it.  Any other method is dbt lsp's alone.
pub static CAPABILITIES: [Capability; 18] = [
    Capability { name: "textDocument/documentSymbol", key: Some("documentSymbolProvider"), added: true, ..DEFAULT },
    Capability { name: "workspace/symbol", key: Some("workspaceSymbolProvider"), added: true, list: true, ..DEFAULT },
    Capability {
        name: "textDocument/prepareCallHierarchy",
        key: Some("callHierarchyProvider"),
        added: true,
        reads_project: true,
        ..DEFAULT
    },
    Capability { name: "callHierarchy/incomingCalls", leader: Some("textDocument/prepareCallHierarchy"), ..DEFAULT },
    Capability { name: "callHierarchy/outgoingCalls", leader: Some("textDocument/prepareCallHierarchy"), ..DEFAULT },
    Capability { name: "textDocument/foldingRange", key: Some("foldingRangeProvider"), added: true, ..DEFAULT },
    Capability {
        name: "textDocument/documentHighlight",
        key: Some("documentHighlightProvider"),
        added: true,
        ..DEFAULT
    },
    Capability { name: "textDocument/selectionRange", key: Some("selectionRangeProvider"), added: true, ..DEFAULT },
    Capability {
        name: "textDocument/hover",
        key: Some("hoverProvider"),
        combine: Combine::Sections,
        reads_project: true,
        ..DEFAULT
    },
    Capability {
        name: "textDocument/inlayHint",
        key: Some("inlayHintProvider"),
        combine: Combine::Append,
        list: true,
        reads_project: true,
        ..DEFAULT
    },
    Capability {
        name: "textDocument/definition",
        key: Some("definitionProvider"),
        combine: Combine::Union,
        wants_index: true,
        kinds: &[
            ("source", &["sql", "yaml", "python"]),
            ("ref", &["sql", "yaml", "python"]),
            ("macro", &["sql", "yaml"]),
            ("doc", &["yaml"]),
            ("entry", &["yaml"]),
            ("test", &["yaml"]),
            ("function", &["sql", "yaml"]),
        ],
        reads_project: true,
        ..DEFAULT
    },
    Capability {
        name: "textDocument/references",
        key: Some("referencesProvider"),
        combine: Combine::Union,
        list: true,
        wants_index: true,
        kinds: &[
            ("source", &["sql", "yaml", "python"]),
            ("ref", &["sql", "yaml", "python"]),
            ("macro", &["sql", "yaml"]),
            ("function", &["sql", "yaml"]),
        ],
        reads_project: true,
        ..DEFAULT
    },
    Capability {
        name: "textDocument/rename",
        key: Some("renameProvider"),
        kinds: &[("source", &["sql", "yaml", "python"]), ("cte", &["sql"])],
        ..DEFAULT
    },
    Capability { name: "textDocument/publishDiagnostics", combine: Combine::PerFile, ..DEFAULT },
    Capability { name: NODE_INFO, providers: &[Provider::Plus], wants_index: true, ..DEFAULT },
    Capability {
        name: COMPILE,
        providers: &[Provider::DbtLsp, Provider::DbtCli],
        server: Some("dbt.compileFile"),
        ..DEFAULT
    },
    // `dbt show` alone would reach the network.
    Capability { name: SHOW, providers: &[Provider::DbtLsp], server: Some("dbt.show"), ..DEFAULT },
    Capability {
        name: NODES,
        providers: &[Provider::DbtLsp, Provider::DbtCli],
        server: Some("dbt.listNodes"),
        ..DEFAULT
    },
];

impl Capability {
    /// Whether it is a command: its name holds no `/`, as every LSP
    /// method's does, so that a command named like a method is no method.
    pub fn is_command(&self) -> bool {
        !self.name.contains('/')
    }

    /// Whether it is a method the proxy does not add, announced by a key,
    /// with no kinds of target: hover and inlay hints, which the proxy
    /// answers whole when alone.
    pub fn whole(&self) -> bool {
        !self.added && self.key.is_some() && self.kinds.is_empty()
    }

    /// How long a merged request waits for dbt lsp before the client gets
    /// the proxy's answer alone: `MERGE_DEADLINE` when the proxy has a
    /// PART, `NOTHING_DEADLINE` when it has nothing; none for a rule that
    /// waits for no one.
    pub fn deadline(&self, part: bool) -> Option<Duration> {
        let waits = matches!(self.combine, Combine::Union | Combine::Sections | Combine::Append);
        waits.then_some(if part { MERGE_DEADLINE } else { NOTHING_DEADLINE })
    }
}

/// The capability named NAME, if the proxy has one.
pub fn of(name: &str) -> Option<&'static Capability> {
    CAPABILITIES.iter().find(|capability| capability.name == name)
}

/// The methods that take a mode, in the table's order: every LSP method
/// of the table but a follower, which takes its leader's.
pub fn with_mode() -> impl Iterator<Item = &'static str> {
    let takes = |capability: &&Capability| !capability.is_command() && capability.leader.is_none();
    CAPABILITIES.iter().filter(takes).map(|capability| capability.name)
}

/// What NAME answers when it has nothing: `[]` for a list, else null.
pub fn empty(name: &str) -> Value {
    if of(name).is_some_and(|capability| capability.list) { json!([]) } else { Value::Null }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reference::Kind;

    #[test]
    fn a_command_is_found_by_its_name_and_a_method_is_no_command() {
        let compile = of(COMPILE).expect("the compile");
        assert!(compile.is_command());
        assert_eq!(compile.server, Some("dbt.compileFile"));
        assert!(of("textDocument/hover").is_some_and(|hover| !hover.is_command()));
        assert!(of("textDocument/completion").is_none(), "dbt lsp's alone");
        assert!(!with_mode().any(|name| name == NODE_INFO || name == "callHierarchy/incomingCalls"));
        assert_eq!(with_mode().count(), 12);
    }

    #[test]
    fn the_reference_page_shows_each_capability() {
        let page = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/reference/features.md")).unwrap();
        let heading = "### Modes";
        let section =
            page.lines().skip_while(|line| *line != heading).skip(1).take_while(|line| !line.starts_with('#'));
        let table = section.skip_while(|line| !line.starts_with('|')).take_while(|line| line.starts_with('|'));
        let cells = |line: &str| -> Vec<String> {
            let cells = line.trim().trim_matches('|').split('|');
            cells.map(|cell| cell.trim_matches(|c| c == ' ' || c == '`').to_owned()).collect()
        };
        let rows: Vec<Vec<String>> = table.map(cells).collect();
        let columns = ["Method or command", "Answered by", "Shipped mode", "Combined", "Waits for dbt lsp"];
        assert_eq!(rows.first().map(Vec::as_slice), Some(&columns.map(str::to_owned)[..]), "the table under {heading}");
        let names = [(Provider::Plus, "dbt-lsp-plus"), (Provider::DbtLsp, "dbt lsp"), (Provider::DbtCli, "dbt's CLI")];
        let rules = [
            (Combine::Union, "the union, dbt-lsp-plus's first"),
            (Combine::Sections, "dbt-lsp-plus's text, then dbt lsp's under its name"),
            (Combine::Append, "dbt lsp's, then dbt-lsp-plus's"),
            (Combine::PerFile, "both, per file"),
        ];
        let rendered = CAPABILITIES.iter().map(|capability| {
            let named = |provider: &Provider| names.iter().find(|(named, _)| named == provider).map(|(_, name)| *name);
            let providers: Vec<&str> = capability.providers.iter().filter_map(named).collect();
            let leader = capability.leader.map(|leader| format!("{}'s", leader.trim_start_matches("textDocument/")));
            let shipped = match (capability.is_command(), capability.added) {
                (true, _) => "none".to_owned(),
                (false, true) => "plus, or dbt where dbt lsp announces it".to_owned(),
                (false, false) => leader.unwrap_or_else(|| "merge".to_owned()),
            };
            let first = match (capability.providers.contains(&Provider::Plus), capability.kinds.is_empty()) {
                (false, _) => "the first present",
                (true, true) => "dbt-lsp-plus's",
                (true, false) => "dbt-lsp-plus's at its kinds, else dbt lsp's",
            };
            let rule = rules.iter().find(|(rule, _)| *rule == capability.combine).map_or(first, |(_, said)| *said);
            let waits = capability.deadline(true).zip(capability.deadline(false));
            let waits = waits.map_or("no".to_owned(), |(part, nothing)| {
                format!("{} ms, {} s with nothing", part.as_millis(), nothing.as_secs())
            });
            [capability.name.to_owned(), providers.join(", "), shipped, rule.to_owned(), waits].to_vec()
        });
        assert_eq!(rows.get(2..), Some(&rendered.collect::<Vec<_>>()[..]), "the rows under {heading}");
    }

    #[test]
    fn each_kind_shows_today_s_word_and_each_listed_word_is_a_kind_s() {
        // The word that each kind's position showed before plan 045, and a dispatch's, which no position names.
        let mut words = vec![
            (Kind::Ref, "ref"),
            (Kind::UnitTestModel, "ref"),
            (Kind::Table { source: "raw".into() }, "source"),
            (Kind::Source, "source"),
            (Kind::Macro, "macro"),
            (Kind::Function, "function"),
            (Kind::Doc, "doc"),
            (Kind::Test, "test"),
            (Kind::Dispatch, "dispatch"),
        ];
        // An entry of each label, a macro's and a function's included.
        let labels = ["model", "seed", "snapshot", "analysis", "macro", "function"];
        words.extend(labels.map(|label| (Kind::Entry { label }, "entry")));
        for (kind, word) in &words {
            assert_eq!(kind.flag(), *word, "{kind:?}");
        }
        // A CTE is local to its document, no Name.
        for capability in &CAPABILITIES {
            let listed = capability.kinds.iter().map(|(word, _)| *word);
            for word in listed.filter(|word| !(capability.name == "textDocument/rename" && *word == "cte")) {
                assert!(words.iter().any(|(_, shown)| *shown == word), "{} lists {word}", capability.name);
            }
        }
    }

    #[test]
    fn a_merge_that_waits_for_dbt_lsp_has_both_deadlines() {
        let waiting =
            ["textDocument/hover", "textDocument/inlayHint", "textDocument/definition", "textDocument/references"];
        for capability in &CAPABILITIES {
            let deadlines = [true, false].map(|part| capability.deadline(part));
            let expected = match waiting.contains(&capability.name) {
                true => [Some(Duration::from_millis(300)), Some(Duration::from_secs(2))],
                false => [None, None],
            };
            assert_eq!(deadlines, expected, "{}", capability.name);
        }
    }
}
