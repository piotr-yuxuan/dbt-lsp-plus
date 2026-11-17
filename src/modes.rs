// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! Who answers each method: the proxy alone (`ours`), `dbt lsp` alone
//! (`dbt`), or both, merged (`merge`).  The user's settings, from the
//! command line and `initializationOptions.dbtLspPlus`, parsed here at the
//! boundary; the table of a session, fixed once `initialize` is answered.

use regex::Regex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

/// Who answers a method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Ours,
    Dbt,
    Merge,
}

impl Mode {
    /// The mode that TEXT names, if any.
    pub fn parse(text: &str) -> Option<Mode> {
        match text {
            "ours" => Some(Mode::Ours),
            "dbt" => Some(Mode::Dbt),
            "merge" => Some(Mode::Merge),
            _ => None,
        }
    }
}

/// The methods that take a mode: those the proxy answers, by their LSP
/// names.  Any other stays `dbt`.
pub const METHODS: [&str; 12] = [
    "textDocument/documentSymbol",
    "workspace/symbol",
    "textDocument/prepareCallHierarchy",
    "textDocument/foldingRange",
    "textDocument/documentHighlight",
    "textDocument/selectionRange",
    "textDocument/hover",
    "textDocument/inlayHint",
    "textDocument/definition",
    "textDocument/references",
    "textDocument/rename",
    "textDocument/publishDiagnostics",
];

/// The methods that take another's mode, as their items come from it.
const FOLLOWERS: [(&str, &str); 2] = [
    ("callHierarchy/incomingCalls", "textDocument/prepareCallHierarchy"),
    ("callHierarchy/outgoingCalls", "textDocument/prepareCallHierarchy"),
];

/// The methods whose shipped default is `ours` when the first server does
/// not announce their capability, else `dbt`: those the proxy adds.  Every
/// other method of `METHODS` ships `merge`, today's merges.
const ADDED: [&str; 6] = [
    "textDocument/documentSymbol",
    "workspace/symbol",
    "textDocument/prepareCallHierarchy",
    "textDocument/foldingRange",
    "textDocument/documentHighlight",
    "textDocument/selectionRange",
];

/// The methods whose empty answer is `[]` rather than null.
const LISTS: [&str; 3] = ["textDocument/references", "textDocument/inlayHint", "workspace/symbol"];

/// A method of LSP: a namespace of the protocol's and a name.
static LSP_METHOD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\A(textDocument|workspace|callHierarchy|typeHierarchy|notebookDocument|completionItem|codeAction|codeLens|documentLink|inlayHint|workspaceSymbol)/[A-Za-z]+\z",
    )
    .unwrap()
});

/// The user's settings: a mode per method of `METHODS`, and a default for
/// them all.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub modes: BTreeMap<String, Mode>,
    pub default: Option<Mode>,
}

impl Settings {
    /// Sets METHOD to the mode MODE names, or says why it cannot be.  `dbt`
    /// for a method the proxy does not answer is its behaviour already, and
    /// accepted.
    pub fn set(&mut self, method: &str, mode: &str) -> Result<(), String> {
        let named = Mode::parse(mode).ok_or_else(|| format!("{mode} is no mode: ours, dbt or merge"))?;
        if let Some((_, leader)) = FOLLOWERS.iter().find(|(follower, _)| *follower == method) {
            return Err(format!("{method} takes the mode of {leader}"));
        }
        match (METHODS.contains(&method), LSP_METHOD.is_match(method), named) {
            (true, ..) => drop(self.modes.insert(method.to_owned(), named)),
            (false, true, Mode::Dbt) => {}
            (false, true, _) => return Err(format!("{method} has no answer of dbt-lsp-plus's: dbt only")),
            (false, false, _) => return Err(format!("{method} is no LSP method")),
        }
        Ok(())
    }

    /// Sets the default to the mode MODE names, or says why it cannot be.
    pub fn set_default(&mut self, mode: &str) -> Result<(), String> {
        self.default = Some(Mode::parse(mode).ok_or_else(|| format!("{mode} is no mode: ours, dbt or merge"))?);
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
            "backend" => "dbtLspPlus.backend is read from the command line only (--backend)".to_owned(),
            key => format!("dbtLspPlus.{key} is not read: modes and defaultMode are"),
        }));
        (settings, refused)
    }
}

/// `initialize`'s PARAMS without the proxy's own section, as the server got
/// them before modes: `initializationOptions` without `dbtLspPlus`, none
/// when nothing else remains.
pub fn without_section(params: &Value) -> Value {
    let mut params = params.clone();
    let options = params.get_mut("initializationOptions").and_then(Value::as_object_mut);
    let emptied = options.is_some_and(|options| options.remove("dbtLspPlus").is_some() && options.is_empty());
    if emptied && let Some(params) = params.as_object_mut() {
        params.remove("initializationOptions");
    }
    params
}

/// Whether the server runs behind the proxy: `auto`, when the dbt has
/// `lsp`, or `never`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    #[default]
    Auto,
    Never,
}

/// What the proxy's command line asks.
#[derive(Debug, PartialEq)]
pub enum Arguments {
    Help,
    Version,
    Run { settings: Settings, backend: Backend, command: Vec<String> },
}

/// The options that print and exit: true for the usage, false for the version.
const ASKED: [(&str, bool); 4] = [("-h", true), ("--help", true), ("-V", false), ("--version", false)];

/// The backends that `--backend` names.
const BACKENDS: [(&str, Backend); 2] = [("auto", Backend::Auto), ("never", Backend::Never)];

/// The proxy's command line, WORDS, after its name: its options, read from
/// the first word for as long as one starts with `-` and is not `--`, each
/// value the next word or after the first `=`; then the server command,
/// `dbt lsp` when none.  Or why it cannot be read.
pub fn arguments(words: &[String]) -> Result<Arguments, String> {
    let (mut settings, mut backend) = (Settings::default(), Backend::Auto);
    let mut rest = words.iter().peekable();
    while let Some(word) = rest.next_if(|word| word.starts_with('-') && *word != "--") {
        let (option, attached) = word.split_once('=').map_or((word.as_str(), None), |(o, v)| (o, Some(v.to_owned())));
        let asked = ASKED.iter().find(|(name, _)| *name == option).map(|(_, help)| *help);
        if let Some(help) = asked {
            return Ok(if help { Arguments::Help } else { Arguments::Version });
        }
        let value = attached.or_else(|| rest.next().cloned()).ok_or(format!("{option} needs a value"))?;
        let assigned = || value.split_once('=').ok_or(format!("--mode {value}: METHOD=MODE"));
        let named = BACKENDS.iter().find(|(name, _)| *name == value).map(|(_, named)| *named);
        let set = match option {
            "--mode" => assigned().and_then(|(method, mode)| settings.set(method, mode)),
            "--default-mode" => settings.set_default(&value),
            "--backend" => named.map(|named| backend = named).ok_or(format!("{value} is no backend: auto or never")),
            other => Err(format!("{other} is no option of dbt-lsp-plus")),
        };
        set?;
    }
    let command: Vec<String> = rest.skip_while(|word| *word == "--").cloned().collect();
    let command =
        Some(command).filter(|command| !command.is_empty()).unwrap_or_else(|| vec!["dbt".into(), "lsp".into()]);
    Ok(Arguments::Run { settings, backend, command })
}

/// The mode of each method of `METHODS` in a session, fixed when its
/// `initialize` is answered, and whether it has no backend.
#[derive(Clone, Debug, Default)]
pub struct Table {
    modes: HashMap<&'static str, Mode>,
    none: bool,
}

impl Table {
    /// The table of SETTINGS, ANNOUNCED saying whether the first server
    /// announces a method's capability, and NONE whether there is no
    /// backend: the user's mode for a method, else the user's default, else
    /// the shipped one, today's behaviour.
    pub fn new(settings: &Settings, announced: &dyn Fn(&str) -> bool, none: bool) -> Table {
        let shipped = |method: &str| match (ADDED.contains(&method), announced(method)) {
            (true, false) => Mode::Ours,
            (true, true) => Mode::Dbt,
            (false, _) => Mode::Merge,
        };
        let mode = |method: &'static str| {
            let set = settings.modes.get(method).copied().or(settings.default);
            (method, set.unwrap_or_else(|| shipped(method)))
        };
        Table { modes: METHODS.into_iter().map(mode).collect(), none }
    }

    /// The mode of METHOD: a follower's leader's, `dbt` for a method that
    /// takes none.
    pub fn mode(&self, method: &str) -> Mode {
        let leader = FOLLOWERS.iter().find(|(follower, _)| *follower == method).map_or(method, |(_, leader)| leader);
        self.modes.get(leader).copied().unwrap_or(Mode::Dbt)
    }

    /// Whether the proxy answers METHOD at a position it claims, by a row of
    /// `experimental.dbtLspPlus`, ALONE whether that row holds with no
    /// backend only: every row in `ours`, today's in `merge`, none in `dbt`.
    pub fn lists(&self, method: &str, alone: bool) -> bool {
        match self.mode(method) {
            Mode::Ours => true,
            Mode::Merge => self.none || !alone,
            Mode::Dbt => false,
        }
    }

    /// What the proxy answers METHOD where it has nothing of its own, if it
    /// answers it: null, or `[]` for a list, in `ours`, and in `dbt` with no
    /// backend; none where the server answers.
    pub fn nothing(&self, method: &str) -> Option<Value> {
        let mode = self.mode(method);
        let empty = METHODS.contains(&method) && (mode == Mode::Ours || (self.none && mode == Mode::Dbt));
        empty.then(|| if LISTS.contains(&method) { json!([]) } else { Value::Null })
    }

    /// `experimental.dbtLspPlus`, version 2, from ROWS (request, kind,
    /// places, alone only): each row the table lists, its places in order.
    pub fn flag(&self, rows: &[(&str, &str, &[&str], bool)]) -> Value {
        let mut answered = json!({"version": 2});
        for (request, kind, places, alone) in rows {
            if self.lists(&format!("textDocument/{request}"), *alone) {
                let listed = answered[*request][*kind].as_array().cloned().unwrap_or_default();
                answered[*request][*kind] = json!([listed, places.iter().map(|place| json!(place)).collect()].concat());
            }
        }
        answered
    }

    /// The methods set to `dbt` that no backend can answer.
    pub fn unanswered(&self) -> Vec<&'static str> {
        let unanswered = METHODS.into_iter().filter(|method| self.none && self.mode(method) == Mode::Dbt);
        unanswered.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    fn settings(modes: &[(&str, &str)], default: Option<&str>) -> Settings {
        let mut settings = Settings::default();
        for (method, mode) in modes {
            settings.set(method, mode).unwrap();
        }
        default.into_iter().for_each(|mode| settings.set_default(mode).unwrap());
        settings
    }

    #[test]
    fn a_method_takes_a_mode_when_the_proxy_answers_it() {
        let mut settings = Settings::default();
        assert_eq!(settings.set("textDocument/hover", "dbt"), Ok(()));
        assert_eq!(settings.modes["textDocument/hover"], Mode::Dbt);
        let refused = |method: &str, mode: &str| Settings::default().set(method, mode).unwrap_err();
        assert!(refused("textDocument/hover", "theirs").contains("no mode"));
        assert!(refused("callHierarchy/incomingCalls", "ours").contains("prepareCallHierarchy"));
        assert!(refused("textDocument/completion", "ours").contains("dbt only"));
        assert!(refused("hover", "dbt").contains("no LSP method"));
        assert_eq!(Settings::default().set("textDocument/completion", "dbt"), Ok(()), "already its behaviour");
    }

    #[test]
    fn the_shipped_defaults_are_today_s_behaviour() {
        let announced = |method: &str| method == "textDocument/foldingRange";
        let table = Table::new(&Settings::default(), &announced, false);
        assert_eq!(table.mode("textDocument/documentSymbol"), Mode::Ours, "a capability the server lacks");
        assert_eq!(table.mode("textDocument/foldingRange"), Mode::Dbt, "one it announces");
        assert_eq!(table.mode("callHierarchy/outgoingCalls"), Mode::Ours, "its leader's");
        for merged in ["textDocument/hover", "textDocument/definition", "textDocument/publishDiagnostics"] {
            assert_eq!(table.mode(merged), Mode::Merge, "{merged}");
        }
        assert_eq!(table.mode("textDocument/completion"), Mode::Dbt);
    }

    #[test]
    fn a_method_s_mode_comes_before_the_default_and_the_default_before_the_shipped_one() {
        let user = settings(&[("textDocument/hover", "dbt")], Some("ours"));
        let table = Table::new(&user, &|_| false, false);
        assert_eq!(table.mode("textDocument/hover"), Mode::Dbt);
        assert_eq!(table.mode("textDocument/definition"), Mode::Ours);
        assert_eq!(table.mode("textDocument/completion"), Mode::Dbt, "the default reaches the proxy's methods only");
    }

    #[test]
    fn initialization_options_go_over_the_command_line_method_by_method() {
        let command_line = settings(&[("textDocument/hover", "dbt")], Some("merge"));
        let options = json!({"dbtLspPlus": {"modes": {"textDocument/definition": "ours"}}});
        let (merged, refused) = command_line.over(&options);
        assert_eq!(refused, Vec::<String>::new());
        assert_eq!(merged.modes["textDocument/hover"], Mode::Dbt);
        assert_eq!(merged.modes["textDocument/definition"], Mode::Ours);
        assert_eq!(merged.default, Some(Mode::Merge));
        let replaced = command_line.over(&json!({"dbtLspPlus": {"defaultMode": "dbt"}})).0;
        assert_eq!(replaced.default, Some(Mode::Dbt));
        for unset in [json!(null), json!({}), json!({"dbtLspPlus": null}), json!({"dbtLspPlus": {"defaultMode": null}})]
        {
            assert_eq!(command_line.over(&unset), (command_line.clone(), Vec::new()), "{unset}");
        }
    }

    #[test]
    fn a_refused_option_is_named_and_the_rest_kept() {
        let options = json!({"dbtLspPlus": {"modes": {"textDocument/hover": "ours", "textDocument/rename": "both",
                                                      "callHierarchy/incomingCalls": "dbt"},
                                            "backend": "never", "colour": "blue"}});
        let (merged, refused) = Settings::default().over(&options);
        assert_eq!(merged.modes.keys().collect::<Vec<_>>(), ["textDocument/hover"]);
        assert_eq!(refused.len(), 4, "{refused:?}");
        assert!(refused.iter().any(|why| why.contains("command line only")));
        assert_eq!(Settings::default().over(&json!({"dbtLspPlus": "ours"})).1.len(), 1);
    }

    #[test]
    fn the_server_gets_initialize_without_the_proxy_s_section() {
        let alone = json!({"rootUri": "file:///p", "initializationOptions": {"dbtLspPlus": {"defaultMode": "dbt"}}});
        assert_eq!(without_section(&alone), json!({"rootUri": "file:///p"}));
        let beside = json!({"initializationOptions": {"dbtLspPlus": {}, "other": 1}});
        assert_eq!(without_section(&beside), json!({"initializationOptions": {"other": 1}}));
        let none = json!({"initializationOptions": {"other": 1}});
        assert_eq!(without_section(&none), none);
    }

    #[test]
    fn the_command_line_reads_options_then_the_server_command() {
        let run = |words: &[&str]| match arguments(&super::tests::words(words)) {
            Ok(Arguments::Run { settings, backend, command }) => (settings, backend, command),
            other => panic!("{other:?}"),
        };
        let (settings, backend, command) =
            run(&["--mode", "textDocument/hover=dbt", "--backend=never", "dbt", "lsp", "--project-dir", "p"]);
        assert_eq!(settings.modes["textDocument/hover"], Mode::Dbt);
        assert_eq!(backend, Backend::Never);
        assert_eq!(command, words(&["dbt", "lsp", "--project-dir", "p"]));
        assert_eq!(run(&["--default-mode=ours"]).0.default, Some(Mode::Ours));
        assert_eq!(run(&[]).2, words(&["dbt", "lsp"]), "dbt lsp by default");
        assert_eq!(run(&["--", "-strange", "lsp"]).2, words(&["-strange", "lsp"]), "-- before a dash");
        assert_eq!(arguments(&words(&["-V", "--mode"])), Ok(Arguments::Version));
        for bad in
            [&["--mode", "hover=dbt"][..], &["--backend", "nevr"], &["--mode"], &["--colour", "x"], &["--mode", "x"]]
        {
            assert!(arguments(&words(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_flag_and_empty_answers_follow_the_modes() {
        let rows: [(&str, &str, &[&str], bool); 3] = [
            ("definition", "source", &["sql", "yaml"], false),
            ("definition", "ref", &["sql"], true),
            ("references", "source", &["sql"], false),
        ];
        let merged = Table::new(&Settings::default(), &|_| false, false);
        assert_eq!(
            merged.flag(&rows),
            json!({"version": 2, "definition": {"source": ["sql", "yaml"]}, "references": {"source": ["sql"]}})
        );
        let alone = Table::new(&Settings::default(), &|_| false, true);
        assert_eq!(alone.flag(&rows)["definition"]["ref"], json!(["sql"]), "alone, today's alone-only rows");
        let user = settings(&[("textDocument/definition", "ours"), ("textDocument/references", "dbt")], None);
        let table = Table::new(&user, &|_| false, false);
        assert_eq!(table.flag(&rows), json!({"version": 2, "definition": {"source": ["sql", "yaml"], "ref": ["sql"]}}));
        assert_eq!(table.nothing("textDocument/definition"), Some(Value::Null));
        assert_eq!(table.nothing("textDocument/references"), None, "dbt with a backend is forwarded");
        let none = Table::new(&user, &|_| false, true);
        assert_eq!(none.nothing("textDocument/references"), Some(json!([])), "dbt with no backend");
        assert_eq!(none.unanswered(), ["textDocument/references"]);
        assert!(table.unanswered().is_empty());
    }
}
