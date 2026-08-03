//! `luaux-lsp` — the LuauX language server.
//!
//! Speaks LSP on stdio. `--version` reports both this server and the compiler it
//! was built against, because those two disagreeing is the failure that is
//! otherwise invisible.

use std::io;

fn main() -> io::Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    for argument in &arguments {
        match argument.as_str() {
            "--version" | "-V" => {
                println!("luaux-lsp {} (luaux {})", luaux_lsp::VERSION, luaux_lsp::LUAUX_VERSION);
                return Ok(());
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            // Accepted and ignored: clients pass it by habit, and stdio is the
            // only transport there is.
            "--stdio" => {}
            other => {
                eprintln!("luaux-lsp: unknown argument {other}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    luaux_lsp::server::run()
}

const USAGE: &str = "\
usage: luaux-lsp [--stdio]

Speaks the Language Server Protocol on stdin and stdout. It owns .luaux files,
answers everything about the markup itself, and forwards Luau questions to a
stock luau-lsp with positions translated in both directions.

  --version   print this server's version and the compiler it was built against
  --help      show this message

Settings come from the editor, not from flags: luaux.server.path,
luaux.luauLsp.path and luaux.trace.server, plus the luau-lsp.* settings the
user already has, which are passed through to the child.";
