// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The errors and warnings of `dbt parse --log-format json`, as dbt 2.0.9
//! and dbt-oss 2.0.11 write them: one JSON object a line on the standard
//! output, which places a problem only in the last line of its message,
//! `  --> PATH[:LINE:COLUMN]`.

use serde_json::Value;

/// Where dbt places a problem: a file, relative to the root of its project
/// or of its package, and its line and column, from 1, the column in code
/// points, when dbt gives them.
#[derive(Clone, Debug, PartialEq)]
pub struct Spot {
    pub path: String,
    pub at: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    /// LSP's: 1 an error, 2 a warning.
    pub severity: u8,
    /// dbt's code, the bare number, as `1048`.
    pub code: String,
    /// dbt's message without its `[error] ` or `[warning] ` prefix, its
    /// spot included.
    pub message: String,
    pub spot: Option<Spot>,
}

/// What a parse's log says.
#[derive(Debug, PartialEq)]
pub struct Logged {
    pub problems: Vec<Problem>,
    /// Whether dbt reached its end, `CommandCompleted` (Q039): a parse
    /// killed or stopped before says nothing of what it did not reach.
    pub completed: bool,
}

/// The problems of LOG, a parse's output, in order.  A line that is no JSON
/// object, as a launcher's own, is skipped, and so is any other event.
pub fn read(log: &str) -> Logged {
    let spot = |message: &str| {
        let pointed = message.lines().last()?.trim_start().strip_prefix("--> ")?;
        let mut parts = pointed.rsplitn(3, ':');
        let (column, line, path) = (parts.next()?, parts.next(), parts.next());
        let at = line.and_then(|line| Some((line.parse().ok()?, column.parse().ok()?)));
        Some(match (at, path) {
            (Some(at), Some(path)) => Spot { path: path.to_owned(), at: Some(at) },
            _ => Spot { path: pointed.to_owned(), at: None },
        })
    };
    let mut read = Logged { problems: Vec::new(), completed: false };
    for event in log.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()) {
        let info = &event["info"];
        read.completed |= info["code"] == "Q039";
        let (severity, prefix) = match info["level"].as_str() {
            Some("error") => (1, "[error] "),
            Some("warn") => (2, "[warning] "),
            _ => continue,
        };
        let Some(message) = info["msg"].as_str() else { continue };
        read.problems.push(Problem {
            severity,
            code: info["code"].as_str().unwrap_or_default().to_owned(),
            message: message.strip_prefix(prefix).unwrap_or(message).to_owned(),
            spot: spot(message),
        });
    }
    read
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A line of the log as both builds write it, with LEVEL, CODE and MSG.
    fn event(level: &str, code: &str, msg: &str) -> String {
        json!({"data": {}, "info": {"category": "", "code": code, "extra": {}, "level": level, "msg": msg,
                                    "name": "Event", "pid": 1, "thread": "tokio-rt-worker"}})
        .to_string()
    }

    #[test]
    fn reads_each_error_and_warning_with_its_place() {
        let log = [
            event("info", "A001", "Running with dbt-oss=2.0.11"),
            "Content-Length: 5".to_owned(),
            event(
                "error",
                "1502",
                "[error] [MacroSyntaxInvalid (dbt1502)]: Failed to render SQL syntax error: \
                                    unexpected end\n(in models/marts/a.sql:2:22)\n  --> models/marts/a.sql:2:22",
            ),
            event(
                "warn",
                "1089",
                "[warning] [NoNodeForYamlKey (dbt1089)]: Unused schema.yml entry for model \
                                   'ghost'\n  --> models/marts/_notes.yml",
            ),
            event(
                "warn",
                "1097",
                "[warning] [UnusedResourceConfigPath (dbt1097)]: Configuration paths exist in \
                                   your dbt_project.yml file which do not apply to any resources.\n- models.shop.x",
            ),
            event("info", "Q039", "Finished 'parse' with 1 error"),
        ]
        .join("\n");
        let read = read(&log);
        assert!(read.completed);
        let jinja = &read.problems[0];
        assert_eq!((jinja.severity, jinja.code.as_str()), (1, "1502"));
        assert!(
            jinja.message.starts_with("[MacroSyntaxInvalid (dbt1502)]: Failed") && jinja.message.ends_with(":2:22")
        );
        assert_eq!(jinja.spot, Some(Spot { path: "models/marts/a.sql".into(), at: Some((2, 22)) }));
        let unused = &read.problems[1];
        assert_eq!(
            (unused.severity, unused.spot.clone()),
            (2, Some(Spot { path: "models/marts/_notes.yml".into(), at: None }))
        );
        assert_eq!(read.problems[2].spot, None, "a warning on no file");
        assert_eq!(read.problems.len(), 3);
    }

    #[test]
    fn a_log_cut_short_is_not_complete() {
        let log = event("error", "1048", "[error] [DependencyNotFound (dbt1048)]: Ref 'x' not found\n  --> m.sql:3:25");
        let cut = read(&log);
        assert!(!cut.completed);
        assert_eq!(cut.problems[0].message, "[DependencyNotFound (dbt1048)]: Ref 'x' not found\n  --> m.sql:3:25");
        assert_eq!(read("").problems, Vec::new());
    }
}
