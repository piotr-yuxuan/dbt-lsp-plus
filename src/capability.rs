// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! What a session offers: one descriptor per method or command.  Routing
//! and the reference's table of methods and commands come from
//! `CAPABILITIES`; what a session announces is written beside it, and a
//! unit test holds it to the rows.

use crate::commands::{COMPILE, NODES};
use crate::config::NODE_INFO;
use serde_json::{Value, json};

/// What the server offers of one LSP method or one command for clients.
#[derive(Debug)]
pub struct Capability {
    pub name: &'static str,
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
const DEFAULT: Capability = Capability { name: "", list: false, wants_index: false, reads_project: false };

/// Every method and command that the server answers.
pub static CAPABILITIES: [Capability; 16] = [
    Capability { name: "textDocument/documentSymbol", ..DEFAULT },
    Capability { name: "workspace/symbol", list: true, ..DEFAULT },
    Capability { name: "textDocument/prepareCallHierarchy", wants_index: true, reads_project: true, ..DEFAULT },
    Capability { name: "callHierarchy/incomingCalls", wants_index: true, ..DEFAULT },
    Capability { name: "callHierarchy/outgoingCalls", wants_index: true, ..DEFAULT },
    Capability { name: "textDocument/foldingRange", ..DEFAULT },
    Capability { name: "textDocument/documentHighlight", ..DEFAULT },
    Capability { name: "textDocument/selectionRange", ..DEFAULT },
    Capability { name: "textDocument/hover", wants_index: true, reads_project: true, ..DEFAULT },
    Capability { name: "textDocument/inlayHint", list: true, reads_project: true, ..DEFAULT },
    Capability { name: "textDocument/definition", wants_index: true, reads_project: true, ..DEFAULT },
    Capability { name: "textDocument/references", list: true, wants_index: true, reads_project: true },
    Capability { name: "textDocument/rename", ..DEFAULT },
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

/// What a session announces: saves, which start a parse, a provider for
/// each method but the call hierarchy's followers, and the commands.
pub fn announced() -> Value {
    json!({"textDocumentSync": {"openClose": true, "change": 2, "save": {}},
           "executeCommandProvider": {"commands": [NODE_INFO, COMPILE, NODES]},
           "documentSymbolProvider": true, "workspaceSymbolProvider": true, "callHierarchyProvider": true,
           "foldingRangeProvider": true, "documentHighlightProvider": true, "selectionRangeProvider": true,
           "hoverProvider": true, "inlayHintProvider": true, "definitionProvider": true, "referencesProvider": true,
           "renameProvider": true})
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
    fn a_provider_is_announced_for_each_method_but_the_call_hierarchy_s_followers() {
        let announced = announced();
        let providers = announced.as_object().unwrap().keys();
        let providers = providers.filter(|key| key.ends_with("Provider") && *key != "executeCommandProvider");
        let methods = CAPABILITIES.iter().filter(|c| !c.is_command() && !c.name.starts_with("callHierarchy/"));
        assert_eq!(providers.count(), methods.count());
        let commands: Vec<&str> = CAPABILITIES.iter().filter(|c| c.is_command()).map(|c| c.name).collect();
        assert_eq!(announced["executeCommandProvider"]["commands"], json!(commands));
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
