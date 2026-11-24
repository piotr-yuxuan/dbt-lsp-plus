// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The server's children: one run of dbt, the model's Run, the parse's and
//! the commands' alike, with its command line, working directory, deadline,
//! log and target, and one reader whose outcome is reported once; what
//! dbt's output says, its first error and a parse's errors and warnings;
//! and `finish`, which waits for a child and kills it past its deadline: a
//! Run's dbt, dbt's probe of `lsp --help` and git through `locate`.

use serde_json::Value;
use std::fs::{self, File};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct Run {
    pub line: Vec<String>,
    /// Where dbt runs; none, the proxy's directory.
    pub cwd: Option<PathBuf>,
    /// Where it writes: its log there, and `target/`.
    pub dir: PathBuf,
    /// The log's name in DIR: `parse.log` or `run.log`.
    pub log: &'static str,
    /// How long it may run before it is killed.
    pub deadline: Duration,
}

#[derive(Debug)]
pub enum Ending {
    Exited(ExitStatus),
    /// It was stopped, or ran past its deadline, and killed.
    Stopped,
    NotStarted(String),
}

/// What a run gave: how it ended; its log, which holds both its outputs, and
/// the log's text; the manifest it wrote, if it wrote one.
#[derive(Debug)]
pub struct Output {
    pub ending: Ending,
    pub log: PathBuf,
    pub text: String,
    pub manifest: Option<Value>,
}

impl Output {
    /// dbt's first error, of either kind of log, since dbt may write the
    /// kind its format does not name, as `design.md` says: an error event's
    /// message, or a line beginning `[error] `, without that prefix.
    pub fn first_error(&self) -> Option<String> {
        let event = |line: &str| problems(line).0.into_iter().find(|problem| problem.severity == 1);
        let error = |line: &str| {
            event(line).map(|problem| problem.message).or(line.strip_prefix("[error] ").map(str::to_owned))
        };
        self.text.lines().map(str::trim).find_map(error)
    }

    pub fn line(&self) -> Option<&str> {
        self.text.lines().map(str::trim).find(|line| !line.is_empty() && !line.starts_with('{'))
    }
}

pub struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Running {
    /// Runs WORK in a thread of its own, which the flag it gets, set when the
    /// run is stopped, asks to end.
    pub fn spawn(work: impl FnOnce(&AtomicBool) + Send + 'static) -> Running {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        Running { stop, thread: Some(thread::spawn(move || work(&stopped))) }
    }

    /// Stops the run, killing dbt if it still runs, and waits for the
    /// thread, which has then reported.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Hands a run's outcome over once: the one the thread sends, else the one
/// it holds, should the thread end without one, as on a panic.
struct Report<T, F: FnOnce(T)>(Option<(F, T)>);

impl<T, F: FnOnce(T)> Drop for Report<T, F> {
    fn drop(&mut self) {
        if let Some((report, ended)) = self.0.take() {
            report(ended);
        }
    }
}

/// Runs RUN in a thread of its own: its directory made and the manifest
/// there removed, so that only this run's counts; dbt started with no input
/// and both its outputs in its log, killed past its deadline or when the
/// run is stopped; then hands REPORT what READ makes of its output, else
/// ENDED, should the thread end without it.
pub fn start<T: Send + 'static>(
    run: Run,
    read: impl FnOnce(&Output) -> T + Send + 'static,
    ended: T,
    report: impl FnOnce(T) + Send + 'static,
) -> Running {
    Running::spawn(move |stopped| {
        let mut report = Report(Some((report, ended)));
        let (log, manifest) = (run.dir.join(run.log), run.dir.join("target/manifest.json"));
        let _ = fs::create_dir_all(&run.dir);
        let _ = fs::remove_file(&manifest);
        let spawned = File::create(&log).and_then(|out| {
            let err = out.try_clone()?;
            let mut child = Command::new(&run.line[0]);
            child.args(&run.line[1..]).stdin(Stdio::null()).stdout(out).stderr(err);
            if let Some(cwd) = &run.cwd {
                child.current_dir(cwd);
            }
            child.spawn()
        });
        let ending = match spawned {
            Ok(child) => finish(child, run.deadline, stopped).map_or(Ending::Stopped, Ending::Exited),
            Err(error) => Ending::NotStarted(format!("cannot run {}: {error}", run.line[0])),
        };
        let text = fs::read_to_string(&log).unwrap_or_default();
        let manifest = fs::read_to_string(&manifest).ok().and_then(|text| serde_json::from_str(&text).ok());
        let read = read(&Output { ending, log, text, manifest });
        if let Some((report, _)) = report.0.take() {
            report(read);
        }
    })
}

/// Waits for CHILD to exit within DEADLINE, unless STOP is set; else kills
/// and reaps it, and gives none.
pub fn finish(mut child: Child, deadline: Duration, stop: &AtomicBool) -> Option<ExitStatus> {
    let end = Instant::now() + deadline;
    while Instant::now() < end && !stop.load(Ordering::Relaxed) {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

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

/// The errors and warnings of LOG, the output of `dbt parse --log-format
/// json` as dbt 2.0.9 and dbt-oss 2.0.11 write it, one JSON object a line,
/// in order; and whether dbt reached its end, `CommandCompleted` (Q039): a
/// parse killed or stopped before says nothing of what it did not reach.  A
/// line that is no JSON object, as a launcher's own, is skipped, and so is
/// any other event.  dbt places a problem only in the last line of its
/// message, `  --> PATH[:LINE:COLUMN]`.
pub fn problems(log: &str) -> (Vec<Problem>, bool) {
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
    let (mut problems, mut completed) = (Vec::new(), false);
    for event in log.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()) {
        let info = &event["info"];
        completed |= info["code"] == "Q039";
        let (severity, prefix) = match info["level"].as_str() {
            Some("error") => (1, "[error] "),
            Some("warn") => (2, "[warning] "),
            _ => continue,
        };
        let Some(message) = info["msg"].as_str() else { continue };
        problems.push(Problem {
            severity,
            code: info["code"].as_str().unwrap_or_default().to_owned(),
            message: message.strip_prefix(prefix).unwrap_or(message).to_owned(),
            spot: spot(message),
        });
    }
    (problems, completed)
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
    fn dbt_s_first_error_is_its_first_error_event_or_error_line_without_its_prefix() {
        let output =
            |text: &str| Output { ending: Ending::Stopped, log: PathBuf::new(), text: text.to_owned(), manifest: None };
        let first = |text: &str| output(text).first_error();
        let version = event("info", "A001", "Running with dbt=2.0.9");
        let warning = event("warn", "1097", "[warning] [UnusedResourceConfigPath (dbt1097)]: unused");
        let jinja = event("error", "1501", "[error] [JinjaError (dbt1501)]: Failed");
        let missing = event("error", "1048", "[error] [DependencyNotFound (dbt1048)]");
        let bare = "[error] [IoError (dbt1001)]: Failed to create output directory: /p/target";
        let (from_event, from_line) = (
            Some("[JinjaError (dbt1501)]: Failed"),
            Some("[IoError (dbt1001)]: Failed to create output directory: /p/target"),
        );
        let events = format!("{version}\n{warning}\n{jinja}\n{missing}\n");
        assert_eq!(first(&events).as_deref(), from_event, "the first error event, past a warning");
        assert_eq!(first(&format!("noise\n  {bare}\n")).as_deref(), from_line, "a text log's line");
        // dbt writes either kind under either format: the first of the two wins.
        assert_eq!(first(&format!("{version}\n{bare}\n{jinja}\n")).as_deref(), from_line, "a line, then an event");
        assert_eq!(first(&format!("{jinja}\n{bare}\n")).as_deref(), from_event, "an event, then a line");
        assert_eq!(first("[error][IoError (dbt1001)]: Failed\n[errors] x\n"), None, "no error without its space");
        assert_eq!(first(&format!("{version}\n{warning}\nnoise\n")), None, "a warning alone");
        assert_eq!(output("\nuvx: no network\n").line(), Some("uvx: no network"));
        assert_eq!(output(&events).line(), None, "only dbt's events");
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
        let (problems, completed) = problems(&log);
        assert!(completed);
        let jinja = &problems[0];
        assert_eq!((jinja.severity, jinja.code.as_str()), (1, "1502"));
        assert!(
            jinja.message.starts_with("[MacroSyntaxInvalid (dbt1502)]: Failed") && jinja.message.ends_with(":2:22")
        );
        assert_eq!(jinja.spot, Some(Spot { path: "models/marts/a.sql".into(), at: Some((2, 22)) }));
        let unused = &problems[1];
        assert_eq!(
            (unused.severity, unused.spot.clone()),
            (2, Some(Spot { path: "models/marts/_notes.yml".into(), at: None }))
        );
        assert_eq!(problems[2].spot, None, "a warning on no file");
        assert_eq!(problems.len(), 3);
    }

    #[test]
    fn a_log_cut_short_is_not_complete() {
        let log = event("error", "1048", "[error] [DependencyNotFound (dbt1048)]: Ref 'x' not found\n  --> m.sql:3:25");
        let (cut, completed) = problems(&log);
        assert!(!completed);
        assert_eq!(cut[0].message, "[DependencyNotFound (dbt1048)]: Ref 'x' not found\n  --> m.sql:3:25");
        assert_eq!(problems("").0, Vec::new());
    }
}
