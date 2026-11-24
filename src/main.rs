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

A language server for dbt on the standard input and output, SERVER COMMAND,
by default `dbt lsp`, naming the user's dbt; each subcommand answers on one
line of JSON and tells more with -h.  `--` is needed only before a server
command that starts with a dash, or whose first word is doctor, select or node.
https://github.com/piotr-yuxuan/dbt-lsp-plus/blob/main/docs/reference/cli.md";

fn main() -> ExitCode {
    let words: Vec<String> = std::env::args().skip(1).collect();
    if let Some(row) = cli::named(&words) {
        return cli::main(row, &words[1..]);
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
