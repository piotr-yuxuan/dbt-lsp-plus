// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy's children but dbt lsp, which is the Backend's (`proxy.rs`):
//! one run of dbt, the model's Run, the parse's and the commands' alike,
//! with its command line, working directory, deadline, log and target, and
//! one reader whose outcome is reported once; and `finish`, which waits for
//! a child and kills it past its deadline: a Run's dbt, dbt's probe of
//! `lsp --help` and git through `locate`, and the cut's probe.

use crate::parse_log;
use serde_json::Value;
use std::fs::{self, File};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// One run of dbt.
pub struct Run {
    /// The command line.
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

/// How a run ended.
#[derive(Debug)]
pub enum Ending {
    /// dbt exited, with this status.
    Exited(ExitStatus),
    /// It was stopped, or ran past its deadline, and killed.
    Stopped,
    /// It could not start, for this reason.
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
    /// dbt's first error: the first error event of a JSON log, else the
    /// first line beginning `[error]`, as a text log writes it.
    pub fn error(&self) -> Option<String> {
        let mut problems = parse_log::read(&self.text).problems.into_iter();
        let event = problems.find(|problem| problem.severity == 1).map(|problem| problem.message);
        let line = || self.text.lines().map(str::trim).find(|line| line.starts_with("[error]")).map(str::to_owned);
        event.or_else(line)
    }

    /// The first line that is none of dbt's events, as a launcher's own.
    pub fn line(&self) -> Option<&str> {
        self.text.lines().map(str::trim).find(|line| !line.is_empty() && !line.starts_with('{'))
    }
}

/// A run under way, which its owner stops at the latest when it ends.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dbt_s_first_error_is_its_first_error_event_else_its_first_error_line() {
        let output =
            |text: &str| Output { ending: Ending::Stopped, log: PathBuf::new(), text: text.to_owned(), manifest: None };
        let event = |level: &str, code: &str, msg: &str| json!({"info": {"code": code, "level": level, "msg": msg}});
        let version = event("info", "A001", "Running with dbt=2.0.9");
        let json = format!(
            "{version}\n{}\n{}\n",
            event("error", "1501", "[error] [JinjaError (dbt1501)]: Failed"),
            event("error", "1048", "[error] [DependencyNotFound (dbt1048)]")
        );
        assert_eq!(output(&json).error().as_deref(), Some("[JinjaError (dbt1501)]: Failed"), "its first, unprefixed");
        let text = "noise\n[error] [IoError (dbt1001)]: Cannot open file\n";
        assert_eq!(output(text).error().as_deref(), Some("[error] [IoError (dbt1001)]: Cannot open file"));
        let no_event = format!("{version}\n[error] x\n");
        assert_eq!(output(&no_event).error().as_deref(), Some("[error] x"), "a JSON log with no error event");
        assert_eq!(output(&format!("{json}[error] x\n")).error(), output(&json).error(), "an error event first");
        assert_eq!(output(&format!("{version}\nnoise\n")).error(), None);
        assert_eq!(output("\nuvx: no network\n").line(), Some("uvx: no network"));
        assert_eq!(output(&json).line(), None, "only dbt's events");
    }
}
