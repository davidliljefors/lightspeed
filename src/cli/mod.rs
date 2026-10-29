//! Developer subcommands: corpus tools, pipeline timing and one-shot queries. The language server
//! itself is `lsp`; everything else here exists for measuring and debugging.

mod analyze;
mod corpus;
mod index;

use lightspeed::workspace::compile_commands::CompileDb;

/// Rayon pool with big stacks (deeply nested C++ recurses in the parser).
fn pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new().num_threads(threads).stack_size(16 << 20).build().expect("thread pool")
}

fn load_cdb(path: &str) -> Option<CompileDb> {
    match CompileDb::load(path) {
        Ok(db) => Some(db),
        Err(e) => {
            eprintln!("{e}");
            None
        }
    }
}

/// Parses `args` (after `-j N`) and runs the subcommand; returns the process exit code.
pub fn run(mut args: Vec<String>) -> i32 {
    let mut threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    if let Some(i) = args.iter().position(|a| a == "-j") {
        match args.get(i + 1).and_then(|n| n.parse().ok()) {
            Some(n) => threads = n,
            None => {
                eprintln!("-j needs a number");
                return 2;
            }
        }
        args.drain(i..=i + 1);
    }
    match args.split_first() {
        Some((cmd, rest)) if cmd == "lex" && !rest.is_empty() => corpus::lex_cmd(threads, rest),
        Some((cmd, rest)) if cmd == "gen" => corpus::gen_cmd(rest),
        Some((cmd, rest)) if cmd == "cdb" => analyze::cdb_cmd(rest),
        Some((cmd, rest)) if cmd == "pp" => analyze::pp_cmd(threads, rest),
        Some((cmd, rest)) if cmd == "parse" => analyze::parse_cmd(threads, rest),
        Some((cmd, rest)) if cmd == "index" => index::index_cmd(threads, rest),
        Some((cmd, rest)) if cmd == "def" => index::def_cmd(threads, rest),
        Some((cmd, rest)) if cmd == "lsp" => {
            let cdb = rest.iter().position(|a| a == "--cdb").and_then(|i| rest.get(i + 1)).map(std::path::PathBuf::from);
            if rayon::ThreadPoolBuilder::new().num_threads(threads).stack_size(16 << 20).build_global().is_err() {
                eprintln!("lightspeed: thread pool already initialised");
            }
            lightspeed::lsp::serve_stdio(cdb, !rest.iter().any(|a| a == "--fuzzy-refs"))
        }
        _ => {
            eprintln!("usage: lightspeed lex [-j threads] <file|dir>... | gen <dir> <files> <bytes> | cdb <compile_commands.json> | pp|parse|index [-j threads] <compile_commands.json> | lsp [--cdb compile_commands.json] [--fuzzy-refs]");
            2
        }
    }
}
