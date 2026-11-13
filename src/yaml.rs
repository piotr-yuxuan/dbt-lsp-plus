// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! A YAML document as a tree whose scalars keep their place in the text.

use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::Marker;

#[derive(Debug, PartialEq)]
pub enum Node {
    /// A scalar, with its line (from 0) and column (in characters, from 0).
    Scalar {
        value: String,
        line: usize,
        column: usize,
    },
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
    /// An alias: dbt's schema files do not need them resolved.
    Alias,
}

impl Node {
    /// The value under KEY of a mapping.
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.entry(key).map(|(_, value)| value)
    }

    /// The key KEY of a mapping, with its value.
    pub fn entry(&self, key: &str) -> Option<(&Node, &Node)> {
        let Node::Map(entries) = self else { return None };
        entries.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(k, value)| (k, value))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Node::Scalar { value, .. } => Some(value),
            _ => None,
        }
    }

    /// The items of a sequence; nothing for any other node.
    pub fn items(&self) -> &[Node] {
        match self {
            Node::Seq(items) => items,
            _ => &[],
        }
    }
}

enum Frame {
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>, Option<Node>),
}

#[derive(Default)]
struct Builder {
    stack: Vec<Frame>,
    root: Option<Node>,
}

impl Builder {
    fn attach(&mut self, node: Node) {
        match self.stack.last_mut() {
            None => self.root = self.root.take().or(Some(node)),
            Some(Frame::Seq(items)) => items.push(node),
            Some(Frame::Map(entries, key)) => match key.take() {
                None => *key = Some(node),
                Some(k) => entries.push((k, node)),
            },
        }
    }
}

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, event: Event, mark: Marker) {
        match event {
            Event::Scalar(value, ..) => {
                self.attach(Node::Scalar { value, line: mark.line().saturating_sub(1), column: mark.col() })
            }
            Event::Alias(_) => self.attach(Node::Alias),
            Event::SequenceStart(..) => self.stack.push(Frame::Seq(Vec::new())),
            Event::MappingStart(..) => self.stack.push(Frame::Map(Vec::new(), None)),
            Event::SequenceEnd | Event::MappingEnd => match self.stack.pop() {
                Some(Frame::Seq(items)) => self.attach(Node::Seq(items)),
                Some(Frame::Map(entries, _)) => self.attach(Node::Map(entries)),
                None => {}
            },
            _ => {}
        }
    }
}

/// The first document of TEXT, or `None` when TEXT is not valid YAML.
pub fn parse(text: &str) -> Option<Node> {
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
        assert_eq!(source.get("name"), Some(&Node::Scalar { value: "événements".into(), line: 1, column: 10 }));
        let table = &source.get("tables").unwrap().items()[0];
        assert_eq!(table.get("name"), Some(&Node::Scalar { value: "page_views".into(), line: 3, column: 14 }));
    }

    #[test]
    fn refuses_invalid_yaml() {
        assert_eq!(parse("a: [unclosed"), None);
        assert_eq!(parse(""), None);
    }
}
