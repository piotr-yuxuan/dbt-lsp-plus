// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! What a session offers: one descriptor per method or command.  The
//! capabilities announced, the commands listed and the reference's table
//! of methods and commands all come from `CAPABILITIES`.

use crate::commands::{COMPILE, NODES};
use crate::config::NODE_INFO;
use serde_json::{Value, json};

/// What the server offers of one LSP method or one command for clients.
#[derive(Debug)]
pub struct Capability {
    pub name: &'static str,
    /// The server capability that announces it; none for a follower of the
    /// call hierarchy, which its first request's announces, and a command.
    pub key: Option<&'static str>,
    /// Its nothing is `[]` rather than null.
    pub list: bool,
    /// Its answer reads the parse's index, which a stale one makes the
    /// Workspace parse again first.
    pub wants_index: bool,
    /// Whether a request on a document of another project gets `Elsewhere`:
    /// a method's, which names it by `textDocument.uri` (`Proxy::outside`),
    /// and nodeInfo's at a position (`Workspace::node`).  Its answer there
    /// would read the session's project, its scan or its index, beyond the
    /// document's text.  Inlay hints never get it: they get `[]`.
    pub reads_project: bool,
}

/// What a row of `CAPABILITIES` does not say.
const DEFAULT: Capability = Capability { name: "", key: None, list: false, wants_index: false, reads_project: false };

/// Every method and command that the server answers.
pub static CAPABILITIES: [Capability; 16] = [
    Capability { name: "textDocument/documentSymbol", key: Some("documentSymbolProvider"), ..DEFAULT },
    Capability { name: "workspace/symbol", key: Some("workspaceSymbolProvider"), list: true, ..DEFAULT },
    Capability {
        name: "textDocument/prepareCallHierarchy",
        key: Some("callHierarchyProvider"),
        wants_index: true,
        reads_project: true,
        ..DEFAULT
    },
    Capability { name: "callHierarchy/incomingCalls", wants_index: true, ..DEFAULT },
    Capability { name: "callHierarchy/outgoingCalls", wants_index: true, ..DEFAULT },
    Capability { name: "textDocument/foldingRange", key: Some("foldingRangeProvider"), ..DEFAULT },
    Capability { name: "textDocument/documentHighlight", key: Some("documentHighlightProvider"), ..DEFAULT },
    Capability { name: "textDocument/selectionRange", key: Some("selectionRangeProvider"), ..DEFAULT },
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
        reads_project: true,
        ..DEFAULT
    },
    Capability {
        name: "textDocument/references",
        key: Some("referencesProvider"),
        list: true,
        wants_index: true,
        reads_project: true,
    },
    Capability { name: "textDocument/rename", key: Some("renameProvider"), ..DEFAULT },
    Capability { name: NODE_INFO, wants_index: true, reads_project: true, ..DEFAULT },
    Capability { name: COMPILE, ..DEFAULT },
    Capability { name: NODES, ..DEFAULT },
];

impl Capability {
    /// Whether it is a command: its name holds no `/`, as every LSP
    /// method's does, so that a command named like a method is no method.
    pub fn is_command(&self) -> bool {
        !self.name.contains('/')
    }
}

pub fn of(name: &str) -> Option<&'static Capability> {
    CAPABILITIES.iter().find(|capability| capability.name == name)
}

/// The capabilities a session announces: saves, which start a parse, each
/// row's key, and the commands for clients.
pub fn announced() -> Value {
    let commands: Vec<&str> = CAPABILITIES.iter().filter(|c| c.is_command()).map(|c| c.name).collect();
    let mut capabilities = json!({"textDocumentSync": {"openClose": true, "change": 2, "save": {}},
                                  "executeCommandProvider": {"commands": commands}});
    for key in CAPABILITIES.iter().filter_map(|c| c.key) {
        capabilities[key] = json!(true);
    }
    capabilities
}

/// What NAME answers when it has nothing: `[]` for a list, else null.
pub fn empty(name: &str) -> Value {
    if of(name).is_some_and(|capability| capability.list) { json!([]) } else { Value::Null }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_is_found_by_its_name_and_a_method_is_no_command() {
        assert!(of(COMPILE).is_some_and(Capability::is_command));
        assert!(of("textDocument/hover").is_some_and(|hover| !hover.is_command()));
        assert!(of("textDocument/completion").is_none(), "no row");
    }

    #[test]
    fn a_session_announces_each_provider_and_the_commands() {
        let providers = [
            "documentSymbolProvider",
            "workspaceSymbolProvider",
            "callHierarchyProvider",
            "foldingRangeProvider",
            "documentHighlightProvider",
            "selectionRangeProvider",
            "hoverProvider",
            "inlayHintProvider",
            "definitionProvider",
            "referencesProvider",
            "renameProvider",
        ];
        let commands = ["dbt-lsp-plus.nodeInfo", "dbt-lsp-plus.compile", "dbt-lsp-plus.nodes"];
        let mut expected = json!({"textDocumentSync": {"openClose": true, "change": 2, "save": {}},
                                  "executeCommandProvider": {"commands": commands}});
        for provider in providers {
            expected[provider] = json!(true);
        }
        assert_eq!(announced(), expected);
    }

    #[test]
    fn the_reference_page_shows_each_capability() {
        let page = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/reference/features.md")).unwrap();
        let heading = "### Methods and commands";
        let section =
            page.lines().skip_while(|line| *line != heading).skip(1).take_while(|line| !line.starts_with('#'));
        let table = section.skip_while(|line| !line.starts_with('|')).take_while(|line| line.starts_with('|'));
        let cells = |line: &str| -> Vec<String> {
            let cells = line.trim().trim_matches('|').split('|');
            cells.map(|cell| cell.trim_matches(|c| c == ' ' || c == '`').to_owned()).collect()
        };
        let rows: Vec<Vec<String>> = table.map(cells).collect();
        let columns = ["Method or command", "Its nothing", "Asks for a parse", "Reads the project"];
        assert_eq!(rows.first().map(Vec::as_slice), Some(&columns.map(str::to_owned)[..]), "the table under {heading}");
        let yes = |flag: bool| if flag { "yes" } else { "no" }.to_owned();
        let rendered = CAPABILITIES.iter().map(|capability| {
            let nothing = if capability.list { "[]" } else { "null" }.to_owned();
            [capability.name.to_owned(), nothing, yes(capability.wants_index), yes(capability.reads_project)].to_vec()
        });
        assert_eq!(rows.get(2..), Some(&rendered.collect::<Vec<_>>()[..]), "the rows under {heading}");
    }
}
