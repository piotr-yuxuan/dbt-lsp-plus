// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! `dbt-lsp-plus [--] [SERVER COMMAND...]`: a language server for dbt on
//! the standard input and output, SERVER COMMAND, `dbt lsp` by default,
//! naming the user's dbt; or, by its first word, one of the subcommands
//! `doctor`, `select` and `node`.

use dbt_lsp_plus::cli::{self, Arguments};
use std::process::ExitCode;

const USAGE: &str = "\
usage: dbt-lsp-plus [--] [SERVER COMMAND...]
       dbt-lsp-plus -h|-V
       dbt-lsp-plus doctor [PATH] [-- ARGUMENTS...]
       dbt-lsp-plus select SELECTOR [-- ARGUMENTS...]
       dbt-lsp-plus node ID|PATH[:LINE[:COLUMN]] [-- ARGUMENTS...]

A language server for dbt on the standard input and output, which answers
from the project's files and the user's `dbt parse`.  SERVER COMMAND, by
default `dbt lsp`, names the user's dbt, by its words before `lsp`, and
the flags its parse takes, after it; it never runs as such.  The project
is the --project-dir of SERVER COMMAND, else the client's root folder when
it holds dbt_project.yml, else the project of the first file opened in
one; until then the server runs nothing.  Without --project-dir, a bare
`dbt` is the project's .venv's, its repository's or its main worktree's,
else the one on PATH, and dbt runs in the project.

  -h, --help     this text
  -V, --version  the version

doctor says, on one line of JSON, which project and which dbt the server
would find from PATH and whether it would work; select lists the nodes that
a dbt selector selects, by `dbt list`; node gives what
the command nodeInfo gives of the node that a unique id or a place in a
file names, from the user's own `dbt parse`.  ARGUMENTS are the server's,
as above; `dbt-lsp-plus doctor -h`, `dbt-lsp-plus select -h` and
`dbt-lsp-plus node -h` tell more.

`--` is needed only before a server command that starts with a dash, or
whose first word is doctor, select or node.";

fn main() -> ExitCode {
    let words: Vec<String> = std::env::args().skip(1).collect();
    if let Some(subcommand) = cli::named(&words) {
        return cli::main(subcommand, &words[1..]);
    }
    match cli::arguments(&words) {
        Ok(Arguments::Help) => println!("{USAGE}"),
        Ok(Arguments::Version) => println!("dbt-lsp-plus {}", env!("CARGO_PKG_VERSION")),
        Ok(Arguments::Run(command)) => return dbt_lsp_plus::proxy::run(&command),
        Err(why) => {
            eprintln!("dbt-lsp-plus: {why}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
