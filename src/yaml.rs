// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! A YAML document as a tree whose scalars keep their place in the text.

use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};

#[derive(Debug, PartialEq)]
pub enum Tree {
    /// A scalar, with whether it was written plain (neither quoted nor a
    /// block), its line (from 0) and column (in characters, from 0).
    Scalar {
        value: String,
        plain: bool,
        line: usize,
        column: usize,
    },
    Seq(Vec<Tree>),
    /// A mapping, with where the parser saw it end: the line (from 0) and
    /// column (in characters, from 0) of its `}` in flow style; in block
    /// style, of the next token, of a lower indentation, or of the end of
    /// the text, the blank and comment lines before it skipped.
    Map {
        entries: Vec<(Tree, Tree)>,
        end: (usize, usize),
    },
    /// An alias: dbt's schema files do not need them resolved.
    Alias,
}

impl Tree {
    /// The value under KEY of a mapping.
    pub fn get(&self, key: &str) -> Option<&Tree> {
        self.entry(key).map(|(_, value)| value)
    }

    /// The key KEY of a mapping, with its value.
    pub fn entry(&self, key: &str) -> Option<(&Tree, &Tree)> {
        let Tree::Map { entries, .. } = self else { return None };
        entries.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(k, value)| (k, value))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Tree::Scalar { value, .. } => Some(value),
            _ => None,
        }
    }

    /// Whether an alias or a merge key appears anywhere in this tree, so that
    /// a value under it may come from elsewhere in its document.
    pub fn shares(&self) -> bool {
        match self {
            Tree::Alias => true,
            Tree::Seq(items) => items.iter().any(Tree::shares),
            Tree::Map { entries, .. } => {
                entries.iter().any(|(key, value)| key.as_str() == Some("<<") || value.shares())
            }
            Tree::Scalar { .. } => false,
        }
    }

    /// Whether a mapping anywhere in this tree names a key twice, where a
    /// reader may take the first value or the last.
    pub fn repeats(&self) -> bool {
        match self {
            Tree::Seq(items) => items.iter().any(Tree::repeats),
            Tree::Map { entries, .. } => {
                let mut keys = std::collections::HashSet::new();
                entries.iter().any(|(key, value)| !keys.insert(key.as_str()) || value.repeats())
            }
            _ => false,
        }
    }

    /// The items of a sequence; nothing for any other tree.
    pub fn items(&self) -> &[Tree] {
        match self {
            Tree::Seq(items) => items,
            _ => &[],
        }
    }
}

enum Frame {
    Seq(Vec<Tree>),
    Map(Vec<(Tree, Tree)>, Option<Tree>),
}

#[derive(Default)]
struct Builder {
    stack: Vec<Frame>,
    root: Option<Tree>,
}

impl Builder {
    fn attach(&mut self, tree: Tree) {
        match self.stack.last_mut() {
            None => self.root = self.root.take().or(Some(tree)),
            Some(Frame::Seq(items)) => items.push(tree),
            Some(Frame::Map(entries, key)) => match key.take() {
                None => *key = Some(tree),
                Some(k) => entries.push((k, tree)),
            },
        }
    }
}

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, event: Event, mark: Marker) {
        match event {
            Event::Scalar(value, style, ..) => self.attach(Tree::Scalar {
                value,
                plain: style == TScalarStyle::Plain,
                line: mark.line().saturating_sub(1),
                column: mark.col(),
            }),
            Event::Alias(_) => self.attach(Tree::Alias),
            Event::SequenceStart(..) => self.stack.push(Frame::Seq(Vec::new())),
            Event::MappingStart(..) => self.stack.push(Frame::Map(Vec::new(), None)),
            Event::SequenceEnd | Event::MappingEnd => match self.stack.pop() {
                Some(Frame::Seq(items)) => self.attach(Tree::Seq(items)),
                Some(Frame::Map(entries, _)) => {
                    self.attach(Tree::Map { entries, end: (mark.line().saturating_sub(1), mark.col()) })
                }
                None => {}
            },
            _ => {}
        }
    }
}

/// The first document of TEXT, or `None` when TEXT is not valid YAML.
pub fn parse(text: &str) -> Option<Tree> {
    let mut builder = Builder::default();
    Parser::new_from_str(text).load(&mut builder, false).ok()?;
    builder.root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_place_of_each_scalar() {
        let text = "sources:\n  - name: \"événements\"\n    tables:\n      - name: page_views\n";
        let root = parse(text).unwrap();
        let source = &root.get("sources").unwrap().items()[0];
        assert_eq!(
            source.get("name"),
            Some(&Tree::Scalar { value: "événements".into(), plain: false, line: 1, column: 10 })
        );
        let table = &source.get("tables").unwrap().items()[0];
        assert_eq!(
            table.get("name"),
            Some(&Tree::Scalar { value: "page_views".into(), plain: true, line: 3, column: 14 })
        );
    }

    #[test]
    fn keeps_whether_a_scalar_is_plain() {
        let text = "plain: a\nsingle: 'a'\ndouble: \"a\"\nliteral: |\n  a\nfolded: >\n  a\n";
        let root = parse(text).unwrap();
        let plain = |key: &str| match root.get(key) {
            Some(Tree::Scalar { plain, .. }) => *plain,
            other => panic!("not a scalar: {other:?}"),
        };
        assert!(plain("plain"));
        for key in ["single", "double", "literal", "folded"] {
            assert!(!plain(key), "{key}");
        }
    }

    fn end(tree: &Tree) -> (usize, usize) {
        let Tree::Map { end, .. } = tree else { panic!("not a mapping: {tree:?}") };
        *end
    }

    #[test]
    fn keeps_where_a_mapping_ends() {
        let text = "models:\n  - name: orders\n    columns:\n      - name: id\n\n  # the next one\n  \
                    - {name: \"é\", columns: [{name: x}]}\nsources:\n  - name: raw\n";
        let root = parse(text).unwrap();
        let models = root.get("models").unwrap().items();
        // In block style, at the next item, past the blank and comment lines.
        assert_eq!(end(&models[0]), (6, 2));
        assert_eq!(end(&models[0].get("columns").unwrap().items()[0]), (6, 2));
        // In flow style, at the brace, in characters.
        assert_eq!(end(&models[1]), (6, 36));
        assert_eq!(end(&models[1].get("columns").unwrap().items()[0]), (6, 34));
        // At the end of the text.
        assert_eq!(end(&root.get("sources").unwrap().items()[0]), (9, 0));
    }

    #[test]
    fn refuses_invalid_yaml() {
        assert_eq!(parse("a: [unclosed"), None);
        assert_eq!(parse(""), None);
    }
}
