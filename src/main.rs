// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! `dbt-lsp-plus [--] [SERVER COMMAND...]`: a language server on the standard
//! input and output, in front of SERVER COMMAND, `dbt lsp` by default.

use std::process::ExitCode;

const USAGE: &str = "\
usage: dbt-lsp-plus [--] [SERVER COMMAND...]

Runs SERVER COMMAND, by default `dbt lsp`, as a language server behind this
one, on the standard input and output.  Every message passes unchanged; the
proxy answers itself documentSymbol, workspace/symbol and the call hierarchy
when the server does not announce them.  The project is the --project-dir of
SERVER COMMAND, else the client's root folder, else the current directory.";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some("-V" | "--version") => {
            println!("dbt-lsp-plus {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--") => {
            args.remove(0);
        }
        _ => {}
    }
    if args.is_empty() {
        args = vec!["dbt".into(), "lsp".into()];
    }
    dbt_lsp_plus::proxy::run(&args)
}
