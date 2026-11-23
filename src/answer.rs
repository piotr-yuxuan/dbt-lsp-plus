// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! What a request gets: found, empty, or unanswered with its reasons, from
//! one closed set whose words live here; and its rendering as a JSON-RPC
//! response.

use crate::capability;
use serde_json::{Value, json};
use std::fmt;
use std::path::PathBuf;

/// LSP's RequestFailed: a request that the server could not answer.
const REQUEST_FAILED: i64 = -32803;

/// What a session sends for one request: the model's Answer.  `Empty` is
/// "nothing to look up": the request's position names no target.  What a
/// lookup gave is `Found`, its nothing included: a target that resolves to
/// nothing is a definition's null, references' `[]`, as the handlers
/// answer; nodeInfo by an id that names no node is `Empty`.  LSP renders
/// both alike; a rendering that tells them apart reads only `Empty` as
/// nothing to look up.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    /// A result, an empty one included.
    Found(Value),
    /// The method's default nothing, `capability::empty`: null, or `[]` for
    /// a list, where there is nothing to look up.
    Empty,
    /// The server could not answer, for these reasons.
    Unanswered(Vec<Reason>),
}

/// Why the server could not answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Reason {
    /// The root's `dbt_project.yml` does not read as a project.
    Unreadable(PathBuf),
    /// No index answers yet: why, as the parse stands.
    Unindexed(String),
    /// A rename refused, as `sources` and `document` write it.
    Refused(String),
    /// The request's document lies in the dbt project at PROJECT, not in
    /// the one this session serves, at SERVED.
    Elsewhere { document: PathBuf, project: PathBuf, served: PathBuf },
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Unreadable(root) => write!(
                f,
                "no dbt project at {}: its dbt_project.yml is missing, is not YAML, or names no project",
                root.display()
            ),
            Reason::Unindexed(why) => write!(f, "dbt parse has not finished: {why}"),
            Reason::Refused(why) => f.write_str(why),
            Reason::Elsewhere { document, project, served } => write!(
                f,
                "{} lies in the dbt project at {}, and this session serves only the one at {}",
                document.display(),
                project.display(),
                served.display()
            ),
        }
    }
}

impl Answer {
    /// A rename's answer: its edit, or its refusal as the reason.
    pub fn of_rename(edit: Result<Value, String>) -> Answer {
        edit.map_or_else(|why| Answer::Unanswered(vec![Reason::Refused(why)]), Answer::Found)
    }

    /// The JSON-RPC response to the request ID of NAME, a method or a
    /// command: its result; for an empty one, `capability::empty(NAME)`; for
    /// an unanswered one, error -32803 whose message is its reasons, joined
    /// by `; `.
    pub fn frame(&self, id: &Value, name: &str) -> Value {
        match self {
            Answer::Found(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Answer::Empty => json!({"jsonrpc": "2.0", "id": id, "result": capability::empty(name)}),
            Answer::Unanswered(reasons) => {
                let message = reasons.iter().map(Reason::to_string).collect::<Vec<_>>().join("; ");
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": REQUEST_FAILED, "message": message}})
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_reason_says_what_it_is() {
        let cases = [
            (
                Reason::Unreadable(PathBuf::from("/work/shop")),
                "no dbt project at /work/shop: its dbt_project.yml is missing, is not YAML, or names no project",
            ),
            (Reason::Unindexed("it is running".into()), "dbt parse has not finished: it is running"),
            (Reason::Refused("another CTE is named final".into()), "another CTE is named final"),
            (
                Reason::Elsewhere {
                    document: PathBuf::from("/r/b/models/m.sql"),
                    project: PathBuf::from("/r/b"),
                    served: PathBuf::from("/r/a"),
                },
                "/r/b/models/m.sql lies in the dbt project at /r/b, and this session serves only the one at /r/a",
            ),
        ];
        for (reason, said) in cases {
            assert_eq!(reason.to_string(), said, "{reason:?}");
        }
    }

    #[test]
    fn an_answer_is_a_result_an_empty_one_or_its_reasons() {
        let id = json!(7);
        let result = |result: Value| json!({"jsonrpc": "2.0", "id": 7, "result": result});
        let (definition, references, symbols) =
            ("textDocument/definition", "textDocument/references", "workspace/symbol");
        let cases = [
            ("a result", Answer::Found(json!([1])), references, result(json!([1]))),
            ("a null result", Answer::Found(Value::Null), references, result(Value::Null)),
            ("an empty list found", Answer::Found(json!([])), references, result(json!([]))),
            ("an empty definition", Answer::Empty, definition, result(Value::Null)),
            ("empty references", Answer::Empty, references, result(json!([]))),
            ("empty symbols", Answer::Empty, symbols, result(json!([]))),
            ("an empty command", Answer::Empty, "dbt-lsp-plus.nodeInfo", result(Value::Null)),
        ];
        for (name, answer, method, expected) in cases {
            let framed = answer.frame(&id, method);
            assert_eq!(framed, expected, "{name}");
            // A result, null included, is never an error: before a project, every request gets null.
            assert!(framed.get("error").is_none() && framed.get("result").is_some(), "{name}: {framed}");
        }
        let unanswered =
            Answer::Unanswered(vec![Reason::Unindexed("it is running".into()), Reason::Refused("no".into())]);
        let framed = unanswered.frame(&id, references);
        let message = "dbt parse has not finished: it is running; no";
        assert_eq!(framed, json!({"jsonrpc": "2.0", "id": 7, "error": {"code": -32803, "message": message}}));
        assert!(framed.get("result").is_none(), "{framed}");
    }
}
