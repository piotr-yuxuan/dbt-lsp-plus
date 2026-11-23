// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! Whether dbt-lsp-plus answers each method: `plus` and `merge`, it does;
//! `dbt`, the method gets its nothing.  The user's settings, from the
//! command line and `initializationOptions.dbtLspPlus`, parsed here at the
//! boundary; the table of a session, fixed once `initialize` is answered.

use crate::capability::{self, CAPABILITIES};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

/// Who answers a method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Plus,
    Dbt,
    Merge,
}

impl Mode {
    /// The mode that TEXT names, if any; `ours`, `plus`'s former name, is
    /// still read.
    pub fn parse(text: &str) -> Option<Mode> {
        match text {
            "plus" | "ours" => Some(Mode::Plus),
            "dbt" => Some(Mode::Dbt),
            "merge" => Some(Mode::Merge),
            _ => None,
        }
    }
}

/// A method of LSP: a namespace of the protocol's and a name.
static LSP_METHOD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\A(textDocument|workspace|callHierarchy|typeHierarchy|notebookDocument|completionItem|codeAction|codeLens|documentLink|inlayHint|workspaceSymbol)/[A-Za-z]+\z",
    )
    .unwrap()
});

/// The user's settings: a mode per method that takes one
/// (`capability::with_mode`), a default for them all, and whether a mode
/// was named `ours`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub modes: BTreeMap<String, Mode>,
    pub default: Option<Mode>,
    pub ours: bool,
}

impl Settings {
    /// Sets METHOD to the mode MODE names, or says why it cannot be.  `dbt`
    /// for a method the proxy does not answer is its behaviour already, and
    /// accepted.  A mode named `ours` is noted.
    pub fn set(&mut self, method: &str, mode: &str) -> Result<(), String> {
        let named = Mode::parse(mode).ok_or_else(|| format!("{mode} is no mode: plus, dbt or merge"))?;
        if let Some(leader) = capability::of(method).and_then(|capability| capability.leader) {
            return Err(format!("{method} takes the mode of {leader}"));
        }
        match (capability::with_mode().any(|taken| taken == method), LSP_METHOD.is_match(method), named) {
            (true, ..) => {
                self.modes.insert(method.to_owned(), named);
                self.ours |= mode == "ours";
            }
            (false, true, Mode::Dbt) => {}
            (false, true, _) => return Err(format!("{method} has no answer of dbt-lsp-plus's: dbt only")),
            (false, false, _) => return Err(format!("{method} is no LSP method")),
        }
        Ok(())
    }

    /// Sets the default to the mode MODE names, or says why it cannot be.
    /// A mode named `ours` is noted.
    pub fn set_default(&mut self, mode: &str) -> Result<(), String> {
        self.default = Some(Mode::parse(mode).ok_or_else(|| format!("{mode} is no mode: plus, dbt or merge"))?);
        self.ours |= mode == "ours";
        Ok(())
    }

    /// These settings with those of OPTIONS, an `initialize`'s
    /// `initializationOptions`, over them: its `dbtLspPlus` section's
    /// `modes`, method by method, and its `defaultMode`; and the reason of
    /// each refusal.  Null, at any level, is unset.
    pub fn over(&self, options: &Value) -> (Settings, Vec<String>) {
        let mut settings = self.clone();
        let mut refused = Vec::new();
        let section = options.get("dbtLspPlus").unwrap_or(&Value::Null);
        let entries = match section {
            Value::Null => return (settings, refused),
            Value::Object(entries) => entries,
            _ => return (settings, vec!["initializationOptions.dbtLspPlus is no object".into()]),
        };
        let modes = entries.get("modes").and_then(Value::as_object).into_iter().flatten();
        for (method, mode) in modes.filter(|(_, mode)| !mode.is_null()) {
            let set = mode.as_str().ok_or_else(|| format!("the mode of {method} is no string"));
            refused.extend(set.and_then(|mode| settings.set(method, mode)).err());
        }
        if let Some(mode) = entries.get("defaultMode").filter(|mode| !mode.is_null()) {
            let set = mode.as_str().ok_or_else(|| "defaultMode is no string".to_owned());
            refused.extend(set.and_then(|mode| settings.set_default(mode)).err());
        }
        let known = |key: &str| ["modes", "defaultMode"].contains(&key);
        let unknown = entries.iter().filter(|(key, value)| !known(key) || (*key == "modes" && !value.is_object()));
        refused.extend(unknown.filter(|(_, value)| !value.is_null()).map(|(key, _)| match key.as_str() {
            "modes" => "dbtLspPlus.modes is no object".to_owned(),
            key => format!("dbtLspPlus.{key} is not read: modes and defaultMode are"),
        }));
        (settings, refused)
    }
}

/// What the proxy's command line asks.
#[derive(Debug, PartialEq)]
pub enum Arguments {
    Help,
    Version,
    Run { settings: Settings, command: Vec<String> },
}

/// The options that print and exit: true for the usage, false for the version.
const ASKED: [(&str, bool); 4] = [("-h", true), ("--help", true), ("-V", false), ("--version", false)];

/// The proxy's command line, WORDS, after its name: its options, read from
/// the first word for as long as one starts with `-` and is not `--`, each
/// value the next word or after the first `=`; then the server command,
/// `dbt lsp` when none.  Or why it cannot be read.
pub fn arguments(words: &[String]) -> Result<Arguments, String> {
    let mut settings = Settings::default();
    let mut rest = words.iter().peekable();
    while let Some(word) = rest.next_if(|word| word.starts_with('-') && *word != "--") {
        let (option, attached) = word.split_once('=').map_or((word.as_str(), None), |(o, v)| (o, Some(v.to_owned())));
        let asked = ASKED.iter().find(|(name, _)| *name == option).map(|(_, help)| *help);
        if let Some(help) = asked {
            return Ok(if help { Arguments::Help } else { Arguments::Version });
        }
        let mut value = || attached.clone().or_else(|| rest.next().cloned()).ok_or(format!("{option} needs a value"));
        let set = match option {
            "--mode" => value().and_then(|value| {
                let (method, mode) = value.split_once('=').ok_or(format!("--mode {value}: METHOD=MODE"))?;
                settings.set(method, mode)
            }),
            "--default-mode" => value().and_then(|value| settings.set_default(&value)),
            other => Err(format!("{other} is no option of dbt-lsp-plus")),
        };
        set?;
    }
    let command: Vec<String> = rest.skip_while(|word| *word == "--").cloned().collect();
    let command =
        Some(command).filter(|command| !command.is_empty()).unwrap_or_else(|| vec!["dbt".into(), "lsp".into()]);
    Ok(Arguments::Run { settings, command })
}

/// The mode of each method that takes one in a session, fixed when its
/// `initialize` is answered.
#[derive(Clone, Debug, Default)]
pub struct Table {
    modes: HashMap<&'static str, Mode>,
}

impl Table {
    /// The table of SETTINGS: the user's mode for a method, else the user's
    /// default, else the shipped one.
    pub fn new(settings: &Settings) -> Table {
        let added = |method: &str| capability::of(method).is_some_and(|capability| capability.added);
        let shipped = |method: &str| if added(method) { Mode::Plus } else { Mode::Merge };
        let mode = |method: &'static str| {
            let set = settings.modes.get(method).copied().or(settings.default);
            (method, set.unwrap_or_else(|| shipped(method)))
        };
        Table { modes: capability::with_mode().map(mode).collect() }
    }

    /// The mode of METHOD: a follower's leader's, `dbt` for a method that
    /// takes none.
    pub fn mode(&self, method: &str) -> Mode {
        self.modes.get(self.leader(method)).copied().unwrap_or(Mode::Dbt)
    }

    /// The method whose mode METHOD takes: its own, or its leader's.
    fn leader<'m>(&self, method: &'m str) -> &'m str {
        capability::of(method).and_then(|capability| capability.leader).unwrap_or(method)
    }

    /// Whether the proxy answers METHOD at a position it claims, by a row of
    /// `experimental.dbtLspPlus`: every row in `plus` and `merge`, none in
    /// `dbt`.
    pub fn lists(&self, method: &str) -> bool {
        self.mode(method) != Mode::Dbt
    }

    /// `experimental.dbtLspPlus`, version 2: the kinds and places of each
    /// capability the table lists, by its request's name.
    pub fn flag(&self) -> Value {
        let mut answered = json!({"version": 2});
        let listed =
            CAPABILITIES.iter().filter(|capability| !capability.kinds.is_empty() && self.lists(capability.name));
        for capability in listed {
            let kinds = capability.kinds.iter().map(|(kind, places)| ((*kind).to_owned(), json!(places)));
            answered[capability.name.trim_start_matches("textDocument/")] = Value::Object(kinds.collect());
        }
        answered
    }

    /// The capabilities a session announces: each method's outside `dbt`,
    /// saves, which start a parse, the flag and the commands for clients.
    pub fn capabilities(&self) -> Value {
        let commands: Vec<&str> = CAPABILITIES.iter().filter(|c| c.is_command()).map(|c| c.name).collect();
        let mut capabilities = json!({"textDocumentSync": {"openClose": true, "change": 2, "save": {}},
                                      "experimental": {"dbtLspPlus": self.flag()},
                                      "executeCommandProvider": {"commands": commands}});
        for key in CAPABILITIES.iter().filter(|c| self.mode(c.name) != Mode::Dbt).filter_map(|c| c.key) {
            capabilities[key] = json!(true);
        }
        capabilities
    }

    /// The methods set to `dbt`, which no one answers.
    pub fn unanswered(&self) -> Vec<&'static str> {
        capability::with_mode().filter(|method| self.mode(method) == Mode::Dbt).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn the_command_line_reads_options_then_the_server_command() {
        let run = |words: &[&str]| match arguments(&super::tests::words(words)) {
            Ok(Arguments::Run { settings, command }) => (settings, command),
            other => panic!("{other:?}"),
        };
        let (settings, command) = run(&["--mode", "textDocument/hover=dbt", "dbt", "lsp", "--project-dir", "p"]);
        assert_eq!(settings.modes["textDocument/hover"], Mode::Dbt);
        assert_eq!(command, words(&["dbt", "lsp", "--project-dir", "p"]));
        assert_eq!(run(&["--default-mode=plus"]).0.default, Some(Mode::Plus));
        assert_eq!(run(&[]).1, words(&["dbt", "lsp"]), "dbt lsp by default");
        assert_eq!(run(&["--", "-strange", "lsp"]).1, words(&["-strange", "lsp"]), "-- before a dash");
        assert_eq!(arguments(&words(&["-V", "--mode"])), Ok(Arguments::Version));
        for bad in
            [&["--mode", "hover=dbt"][..], &["--backend", "never"], &["--mode"], &["--colour", "x"], &["--mode", "x"]]
        {
            assert!(arguments(&words(bad)).is_err(), "{bad:?}");
        }
    }
}
