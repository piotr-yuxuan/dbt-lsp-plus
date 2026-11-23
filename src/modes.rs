// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! Who answers each method: the proxy alone (`plus`), `dbt lsp` alone
//! (`dbt`), or both, merged (`merge`).  The user's settings, from the
//! command line and `initializationOptions.dbtLspPlus`, parsed here at the
//! boundary; the table of a session, fixed once `initialize` is answered.

use crate::capability::{self, CAPABILITIES, Combine};
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
pub enum Launch {
    #[default]
    Auto,
    Never,
}

/// What the proxy's command line asks.
#[derive(Debug, PartialEq)]
pub enum Arguments {
    Help,
    Version,
    Run { settings: Settings, launch: Launch, command: Vec<String> },
}

/// The options that print and exit: true for the usage, false for the version.
const ASKED: [(&str, bool); 4] = [("-h", true), ("--help", true), ("-V", false), ("--version", false)];

/// The launches that `--backend` names.
const LAUNCHES: [(&str, Launch); 2] = [("auto", Launch::Auto), ("never", Launch::Never)];

/// The proxy's command line, WORDS, after its name: its options, read from
/// the first word for as long as one starts with `-` and is not `--`, each
/// value the next word or after the first `=`; then the server command,
/// `dbt lsp` when none.  Or why it cannot be read.
pub fn arguments(words: &[String]) -> Result<Arguments, String> {
    let (mut settings, mut launch) = (Settings::default(), Launch::Auto);
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
            "--backend" => value().and_then(|value| {
                let named = LAUNCHES.iter().find(|(name, _)| *name == value).map(|(_, named)| *named);
                named.map(|named| launch = named).ok_or(format!("{value} is no backend: auto or never"))
            }),
            other => Err(format!("{other} is no option of dbt-lsp-plus")),
        };
        set?;
    }
    let command: Vec<String> = rest.skip_while(|word| *word == "--").cloned().collect();
    let command =
        Some(command).filter(|command| !command.is_empty()).unwrap_or_else(|| vec!["dbt".into(), "lsp".into()]);
    Ok(Arguments::Run { settings, launch, command })
}

/// The mode of each method that takes one in a session, fixed when its
/// `initialize` is answered.
#[derive(Clone, Debug, Default)]
pub struct Table {
    modes: HashMap<&'static str, Mode>,
}

impl Table {
    /// The table of SETTINGS, ANNOUNCED saying whether the first server
    /// announces a method's capability: the user's mode for a method, else
    /// the user's default, else the shipped one, today's behaviour.
    pub fn new(settings: &Settings, announced: &dyn Fn(&str) -> bool) -> Table {
        let added = |method: &str| capability::of(method).is_some_and(|capability| capability.added);
        let shipped = |method: &str| match (added(method), announced(method)) {
            (true, false) => Mode::Plus,
            (true, true) => Mode::Dbt,
            (false, _) => Mode::Merge,
        };
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
    /// `experimental.dbtLspPlus`: every row in `plus` and `merge`, with a
    /// backend or none, none in `dbt`.
    pub fn lists(&self, method: &str) -> bool {
        self.mode(method) != Mode::Dbt
    }

    /// Whether the proxy unites dbt lsp's answer to METHOD with its own: in
    /// `merge`, unless ALONE, with no dbt lsp answering now, for a capability
    /// whose answers are a union of locations.
    pub fn unites(&self, method: &str, alone: bool) -> bool {
        let union = capability::of(method).is_some_and(|capability| capability.combine == Combine::Union);
        union && !alone && self.mode(method) == Mode::Merge
    }

    /// Whether the proxy answers METHOD with the method's nothing where it
    /// has nothing of its own: in `plus`, and in `dbt` when ALONE, with no
    /// dbt lsp answering now; not where the server answers.  The nothing's
    /// shape, null or `[]`, is `Answer::Empty`'s.
    pub fn answers_nothing(&self, method: &str, alone: bool) -> bool {
        let (mode, known) = (self.mode(method), self.modes.contains_key(self.leader(method)));
        known && (mode == Mode::Plus || (alone && mode == Mode::Dbt))
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

    /// The methods set to `dbt` that no one answers when ALONE, with no dbt
    /// lsp answering now.
    pub fn unanswered(&self, alone: bool) -> Vec<&'static str> {
        let unanswered = capability::with_mode().filter(|method| alone && self.mode(method) == Mode::Dbt);
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
        assert!(refused("callHierarchy/incomingCalls", "plus").contains("prepareCallHierarchy"));
        assert!(refused("textDocument/completion", "plus").contains("dbt only"));
        assert!(refused("hover", "dbt").contains("no LSP method"));
        assert_eq!(Settings::default().set("textDocument/completion", "dbt"), Ok(()), "already its behaviour");
    }

    #[test]
    fn ours_is_read_as_plus_and_noted() {
        assert_eq!(Mode::parse("plus"), Some(Mode::Plus));
        let (mut method, mut default) = (Settings::default(), Settings::default());
        method.set("textDocument/hover", "plus").unwrap();
        assert!(!method.ours, "plus is no note");
        method.set("textDocument/hover", "ours").unwrap();
        default.set_default("ours").unwrap();
        assert_eq!((method.modes["textDocument/hover"], method.ours), (Mode::Plus, true));
        assert_eq!((default.default, default.ours), (Some(Mode::Plus), true));
        let mut refused = Settings::default();
        assert!(refused.set("textDocument/completion", "ours").is_err());
        assert!(!refused.ours, "a refused setting notes nothing");
        let named = Settings::default().set_default("theirs");
        assert_eq!(named, Err("theirs is no mode: plus, dbt or merge".to_owned()));
        let options = json!({"dbtLspPlus": {"defaultMode": "ours"}});
        assert!(Settings::default().over(&options).0.ours, "from initializationOptions");
        let options = json!({"dbtLspPlus": {"defaultMode": "plus"}});
        assert!(method.over(&options).0.ours, "from the command line");
    }

    #[test]
    fn the_command_line_reads_ours_as_plus() {
        let read = |words: &[&str]| match arguments(&super::tests::words(words)) {
            Ok(Arguments::Run { settings, .. }) => settings,
            other => panic!("{other:?}"),
        };
        let method = read(&["--mode", "textDocument/hover=ours"]);
        assert_eq!((method.modes["textDocument/hover"], method.ours), (Mode::Plus, true));
        let default = read(&["--default-mode", "ours"]);
        assert_eq!((default.default, default.ours), (Some(Mode::Plus), true));
        assert!(!read(&["--default-mode", "plus"]).ours);
    }

    #[test]
    fn the_shipped_defaults_are_today_s_behaviour() {
        let announced = |method: &str| method == "textDocument/foldingRange";
        let table = Table::new(&Settings::default(), &announced);
        assert_eq!(table.mode("textDocument/documentSymbol"), Mode::Plus, "a capability the server lacks");
        assert_eq!(table.mode("textDocument/foldingRange"), Mode::Dbt, "one it announces");
        assert_eq!(table.mode("callHierarchy/outgoingCalls"), Mode::Plus, "its leader's");
        for merged in ["textDocument/hover", "textDocument/definition", "textDocument/publishDiagnostics"] {
            assert_eq!(table.mode(merged), Mode::Merge, "{merged}");
        }
        assert_eq!(table.mode("textDocument/completion"), Mode::Dbt);
    }

    #[test]
    fn a_method_s_mode_comes_before_the_default_and_the_default_before_the_shipped_one() {
        let user = settings(&[("textDocument/hover", "dbt")], Some("plus"));
        let table = Table::new(&user, &|_| false);
        assert_eq!(table.mode("textDocument/hover"), Mode::Dbt);
        assert_eq!(table.mode("textDocument/definition"), Mode::Plus);
        assert_eq!(table.mode("textDocument/completion"), Mode::Dbt, "the default reaches the proxy's methods only");
    }

    #[test]
    fn initialization_options_go_over_the_command_line_method_by_method() {
        let command_line = settings(&[("textDocument/hover", "dbt")], Some("merge"));
        let options = json!({"dbtLspPlus": {"modes": {"textDocument/definition": "plus"}}});
        let (merged, refused) = command_line.over(&options);
        assert_eq!(refused, Vec::<String>::new());
        assert_eq!(merged.modes["textDocument/hover"], Mode::Dbt);
        assert_eq!(merged.modes["textDocument/definition"], Mode::Plus);
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
        let options = json!({"dbtLspPlus": {"modes": {"textDocument/hover": "plus", "textDocument/rename": "both",
                                                      "callHierarchy/incomingCalls": "dbt"},
                                            "backend": "never", "colour": "blue"}});
        let (merged, refused) = Settings::default().over(&options);
        assert_eq!(merged.modes.keys().collect::<Vec<_>>(), ["textDocument/hover"]);
        assert_eq!(refused.len(), 4, "{refused:?}");
        assert!(refused.iter().any(|why| why.contains("command line only")));
        assert_eq!(Settings::default().over(&json!({"dbtLspPlus": "plus"})).1.len(), 1);
    }

    #[test]
    fn the_command_line_reads_options_then_the_server_command() {
        let run = |words: &[&str]| match arguments(&super::tests::words(words)) {
            Ok(Arguments::Run { settings, launch, command }) => (settings, launch, command),
            other => panic!("{other:?}"),
        };
        let (settings, launch, command) =
            run(&["--mode", "textDocument/hover=dbt", "--backend=never", "dbt", "lsp", "--project-dir", "p"]);
        assert_eq!(settings.modes["textDocument/hover"], Mode::Dbt);
        assert_eq!(launch, Launch::Never);
        assert_eq!(command, words(&["dbt", "lsp", "--project-dir", "p"]));
        assert_eq!(run(&["--default-mode=plus"]).0.default, Some(Mode::Plus));
        assert_eq!(run(&[]).2, words(&["dbt", "lsp"]), "dbt lsp by default");
        assert_eq!(run(&["--", "-strange", "lsp"]).2, words(&["-strange", "lsp"]), "-- before a dash");
        assert_eq!(arguments(&words(&["-V", "--mode"])), Ok(Arguments::Version));
        for bad in
            [&["--mode", "hover=dbt"][..], &["--backend", "nevr"], &["--mode"], &["--colour", "x"], &["--mode", "x"]]
        {
            assert!(arguments(&words(bad)).is_err(), "{bad:?}");
        }
    }

    /// The flag in full: every kind at each of its places.
    fn every() -> Value {
        json!({"version": 2,
            "definition": {"source": ["sql", "yaml", "python"], "ref": ["sql", "yaml", "python"], "macro": ["sql", "yaml"],
                           "doc": ["yaml"], "entry": ["yaml"], "test": ["yaml"], "function": ["sql", "yaml"]},
            "references": {"source": ["sql", "yaml", "python"], "ref": ["sql", "yaml", "python"],
                           "macro": ["sql", "yaml"], "function": ["sql", "yaml"]},
            "rename": {"source": ["sql", "yaml", "python"], "cte": ["sql"]}})
    }

    #[test]
    fn the_flag_lists_every_row_in_plus_and_merge_with_a_backend_or_none() {
        let every = every();
        let mut plus = Settings::default();
        plus.set_default("plus").unwrap();
        for settings in [Settings::default(), plus] {
            assert_eq!(Table::new(&settings, &|_| false).flag(), every, "{settings:?}");
        }
        let mut dbt = Settings::default();
        dbt.set("textDocument/definition", "dbt").unwrap();
        dbt.set("textDocument/references", "dbt").unwrap();
        let rename = json!({"version": 2, "rename": every["rename"]});
        assert_eq!(Table::new(&dbt, &|_| false).flag(), rename);
    }

    #[test]
    fn the_flag_and_empty_answers_follow_the_modes() {
        let every = every();
        assert_eq!(Table::new(&Settings::default(), &|_| false).flag(), every, "merge");
        let user = settings(&[("textDocument/definition", "plus"), ("textDocument/references", "dbt")], None);
        let table = Table::new(&user, &|_| false);
        let definition = json!({"version": 2, "definition": every["definition"], "rename": every["rename"]});
        assert_eq!(table.flag(), definition, "plus lists, dbt does not");
        assert!(table.answers_nothing("textDocument/definition", false));
        assert!(!table.answers_nothing("textDocument/references", false), "dbt with a backend is forwarded");
        let none = Table::new(&user, &|_| false);
        assert!(none.answers_nothing("textDocument/references", true), "dbt with no backend");
        assert_eq!(none.unanswered(true), ["textDocument/references"]);
        let hierarchy = settings(&[("textDocument/prepareCallHierarchy", "dbt")], None);
        let none = Table::new(&hierarchy, &|_| false);
        assert!(none.answers_nothing("callHierarchy/incomingCalls", true), "a follower too");
        assert!(table.unanswered(false).is_empty());
    }
}
