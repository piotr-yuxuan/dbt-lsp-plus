// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! `dbt-lsp-plus [OPTION...] [--] [SERVER COMMAND...]`: a language server on the standard
//! input and output, in front of SERVER COMMAND, `dbt lsp` by default; or, by
//! its first word, one of the subcommands `doctor`, `select` and `node`.

use dbt_lsp_plus::cli;
use dbt_lsp_plus::modes::{self, Arguments};
use std::process::ExitCode;

const USAGE: &str = "\
usage: dbt-lsp-plus [OPTION...] [--] [SERVER COMMAND...]
       dbt-lsp-plus doctor [PATH] [-- ARGUMENTS...]
       dbt-lsp-plus select SELECTOR [-- ARGUMENTS...]
       dbt-lsp-plus node ID|PATH[:LINE[:COLUMN]] [-- ARGUMENTS...]

Runs SERVER COMMAND, by default `dbt lsp`, as a language server behind this
one, on the standard input and output.  Every message passes unchanged; the
proxy answers itself documentSymbol, workspace/symbol and the call hierarchy
when the server does not announce them.  With a dbt that has no `lsp`, such
as dbt-oss, it answers alone, from the files and the user's `dbt parse`.
The project is the --project-dir of SERVER COMMAND, else the client's root
folder when it holds dbt_project.yml, else the project of the first file
opened in one; until then the proxy runs nothing.  Without --project-dir,
a bare `dbt` is the project's .venv's, its repository's or its main
worktree's, else the one on PATH, and dbt runs in the project.

  --mode METHOD=MODE    who answers the LSP method METHOD: plus (the proxy
                        alone), dbt (dbt lsp alone) or merge (both)
  --default-mode MODE   the mode of every method the proxy answers but
                        those set with --mode
  --backend auto|never  never: no dbt lsp, the proxy alone
  --shadow              compare the proxy's definitions and references
                        with dbt lsp's, in a local log
  -h, --help            this text
  -V, --version         the version

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
    match modes::arguments(&words) {
        Ok(Arguments::Help) => println!("{USAGE}"),
        Ok(Arguments::Version) => println!("dbt-lsp-plus {}", env!("CARGO_PKG_VERSION")),
        Ok(Arguments::Run { settings, launch, command }) => {
            return dbt_lsp_plus::proxy::run(&command, settings, launch);
        }
        Err(why) => {
            eprintln!("dbt-lsp-plus: {why}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
