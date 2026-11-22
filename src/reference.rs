// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! What a place of a document names: its Name, which the scan builds for
//! every call and navigation for a position, and the word that
//! `experimental.dbtLspPlus` shows for its kind.  Pure.

/// What a place of a document names: its kind, the package it gives, if
/// any, the name, and a `ref`'s version.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Name {
    pub kind: Kind,
    pub package: Option<String>,
    /// The called or declared name: a table's for a table of a source.
    pub name: String,
    /// The version that `v=` or `version=` names, as written; empty when it
    /// is no literal, such as `v=var('x')`.  None for any kind but `Ref`.
    pub version: Option<String>,
}

/// The kind of a Name, which says among what dbt looks it up.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `ref('m')` or `ref('p', 'm')`.
    Ref,
    /// A table of SOURCE: `source()`'s second argument, a table's `name:`.
    Table { source: String },
    /// A whole source, a group of tables: `source()`'s first argument, a
    /// source's `name:`.
    Source,
    /// A macro call's name or package; a macro's tag, a generic test's as
    /// `test_<name>`.
    Macro,
    /// `adapter.dispatch('name', 'package')`: the macros `*__name` of the
    /// package.
    Dispatch,
    /// `function('f')` or `function('p', 'f')`: a function of
    /// `function-paths`.
    Function,
    /// `doc('d')` or `doc('p', 'd')`, in a properties file.
    Doc,
    /// A generic test's name in a properties file.
    Test,
    /// A unit test's `model:`.
    UnitTestModel,
    /// The `name:` of an entry of LABEL: `model`, `seed`, `snapshot`,
    /// `analysis`, `macro` or `function`.
    Entry { label: &'static str },
}

impl Name {
    /// The Name of KIND, in PACKAGE if given, with no version.
    pub fn new(kind: Kind, package: Option<&str>, name: &str) -> Name {
        Name { kind, package: package.map(str::to_owned), name: name.to_owned(), version: None }
    }

    /// The source this Name names and, for a table, the table; none for a
    /// name of no source.
    pub fn source_table(&self) -> Option<(&str, Option<&str>)> {
        match &self.kind {
            Kind::Table { source } => Some((source, Some(&self.name))),
            Kind::Source => Some((&self.name, None)),
            _ => None,
        }
    }
}

impl Kind {
    /// Its word in `experimental.dbtLspPlus`: `ref` for a unit test's
    /// `model:` too, `source` for a table or a whole source, `entry` for the
    /// `name:` of any entry, and `dispatch`, which no capability lists.
    pub fn flag(&self) -> &'static str {
        match self {
            Kind::Ref | Kind::UnitTestModel => "ref",
            Kind::Table { .. } | Kind::Source => "source",
            Kind::Macro => "macro",
            Kind::Dispatch => "dispatch",
            Kind::Function => "function",
            Kind::Doc => "doc",
            Kind::Test => "test",
            Kind::Entry { .. } => "entry",
        }
    }
}
