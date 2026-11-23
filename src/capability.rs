// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! What a session offers: one descriptor per method or command.  The
//! capabilities announced, `experimental.dbtLspPlus`, the commands listed
//! and the methods that take a mode all come from `CAPABILITIES`.

use crate::commands::{COMPILE, NODES};
use crate::config::NODE_INFO;
use serde_json::{Value, json};

/// What the server offers of one LSP method or one command for clients.
#[derive(Debug)]
pub struct Capability {
    /// The LSP method, or the command.
    pub name: &'static str,
    /// The server capability that announces it.
    pub key: Option<&'static str>,
    /// The method whose mode it takes, as its items come from it.
    pub leader: Option<&'static str>,
    /// Shipped `plus`; when false, a method ships `merge`.
    pub added: bool,
    /// Its nothing is `[]` rather than null.
    pub list: bool,
    /// Its answer reads the parse's index, which a stale one makes the
    /// Workspace parse again first.
    pub wants_index: bool,
    /// Where the server answers, as `experimental.dbtLspPlus` announces it,
    /// version 2: each kind of target with the places where a request's
    /// position may be, `sql`, `yaml` or `python` by the file's extension.
    /// A kind listed at a place is answered at every position of that kind
    /// there; any other position gets the method's nothing.  Empty for a
    /// method answered whole.
    pub kinds: &'static [(&'static str, &'static [&'static str])],
    /// Whether a request on a document of another project, which it names
    /// by `textDocument.uri`, gets `Elsewhere` (`Proxy::outside`): its
    /// answer there would read the session's project, its scan or its
    /// index, beyond the document's text.  Inlay hints never get it: they
    /// get `[]`.
    pub reads_project: bool,
}

/// What a row of `CAPABILITIES` does not say.
const DEFAULT: Capability = Capability {
    name: "",
    key: None,
    leader: None,
    added: false,
    list: false,
    wants_index: false,
    kinds: &[],
    reads_project: false,
};

/// Every method and command that the server answers, a method's followers
/// after it.
pub static CAPABILITIES: [Capability; 17] = [
    Capability { name: "textDocument/documentSymbol", key: Some("documentSymbolProvider"), added: true, ..DEFAULT },
    Capability { name: "workspace/symbol", key: Some("workspaceSymbolProvider"), added: true, list: true, ..DEFAULT },
    Capability {
        name: "textDocument/prepareCallHierarchy",
        key: Some("callHierarchyProvider"),
        added: true,
        wants_index: true,
        reads_project: true,
        ..DEFAULT
    },
    Capability {
        name: "callHierarchy/incomingCalls",
        leader: Some("textDocument/prepareCallHierarchy"),
        wants_index: true,
        ..DEFAULT
    },
    Capability {
        name: "callHierarchy/outgoingCalls",
        leader: Some("textDocument/prepareCallHierarchy"),
        wants_index: true,
        ..DEFAULT
    },
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
        wants_index: true,
        reads_project: true,
        ..DEFAULT
    },
    Capability {
        name: "textDocument/inlayHint",
        key: Some("inlayHintProvider"),
        list: true,
        reads_project: true,
        ..DEFAULT
    },
    Capability {
        name: "textDocument/definition",
        key: Some("definitionProvider"),
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
    Capability { name: "textDocument/publishDiagnostics", ..DEFAULT },
    Capability { name: NODE_INFO, wants_index: true, ..DEFAULT },
    Capability { name: COMPILE, ..DEFAULT },
    Capability { name: NODES, ..DEFAULT },
];

impl Capability {
    /// Whether it is a command: its name holds no `/`, as every LSP
    /// method's does, so that a command named like a method is no method.
    pub fn is_command(&self) -> bool {
        !self.name.contains('/')
    }

    /// Whether a request of it is answered whole, outside `dbt`: a method
    /// announced by a key, or a follower, with no kinds of target;
    /// `textDocument/publishDiagnostics`, which no request names, is not.
    pub fn whole(&self) -> bool {
        (self.key.is_some() || self.leader.is_some()) && self.kinds.is_empty()
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
        assert!(of(COMPILE).is_some_and(Capability::is_command));
        assert!(of("textDocument/hover").is_some_and(|hover| !hover.is_command()));
        assert!(of("textDocument/completion").is_none(), "no row");
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
        let columns = ["Method or command", "Shipped mode"];
        assert_eq!(rows.first().map(Vec::as_slice), Some(&columns.map(str::to_owned)[..]), "the table under {heading}");
        let rendered = CAPABILITIES.iter().map(|capability| {
            let leader = capability.leader.map(|leader| format!("{}'s", leader.trim_start_matches("textDocument/")));
            let shipped = match (capability.is_command(), capability.added) {
                (true, _) => "none".to_owned(),
                (false, true) => "plus".to_owned(),
                (false, false) => leader.unwrap_or_else(|| "merge".to_owned()),
            };
            [capability.name.to_owned(), shipped].to_vec()
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
}
