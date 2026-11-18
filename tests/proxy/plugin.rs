// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The Claude Code plugin that the repository ships, and its marketplace:
//! a plugin installed at user scope runs in every repository, so its
//! `.lsp.json` holds no path of anyone's.

use serde_json::Value;
use std::path::Path;

/// The JSON file at PATH, from the repository's root.
fn read(path: &str) -> Value {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    serde_json::from_str(&std::fs::read_to_string(&file).unwrap_or_else(|error| panic!("{path}: {error}"))).unwrap()
}

/// Every string in VALUE, its keys aside.
fn strings(value: &Value) -> Vec<&str> {
    match value {
        Value::String(text) => vec![text],
        Value::Array(items) => items.iter().flat_map(strings).collect(),
        Value::Object(entries) => entries.values().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

#[test]
fn the_plugin_starts_dbt_lsp_plus_from_path_and_names_no_path() {
    let marketplace = read(".claude-plugin/marketplace.json");
    let plugins = marketplace["plugins"].as_array().unwrap();
    let plugin = plugins.iter().find(|plugin| plugin["name"] == "dbt-lsp-plus").expect("the plugin dbt-lsp-plus");
    let source = plugin["source"].as_str().unwrap().strip_prefix("./").expect("a source in the repository");
    let manifest = read(&format!("{source}/.claude-plugin/plugin.json"));
    assert_eq!((&manifest["name"], manifest.get("version")), (&plugin["name"], None), "the commit is its version");
    let servers = read(&format!("{source}/.lsp.json"));
    let servers = servers.as_object().unwrap();
    assert_eq!(servers.len(), 1);
    let server = servers.values().next().unwrap();
    assert_eq!(server["command"], "dbt-lsp-plus");
    assert_eq!(server.get("workspaceFolder"), None, "the session's directory, whatever it is");
    let paths: Vec<&str> = strings(server)
        .into_iter()
        .filter(|text| ["/", "\\", "~", "${"].iter().any(|mark| text.contains(mark)))
        .collect();
    assert_eq!(paths, Vec::<&str>::new());
    let mapped: Vec<&String> = server["extensionToLanguage"].as_object().unwrap().keys().collect();
    assert_eq!(mapped, [".sql", ".yaml", ".yml"]);
}
