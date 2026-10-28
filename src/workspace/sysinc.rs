//! Discovery of a compiler's built-in include directories.
//!
//! compile_commands.json normally omits them (clang finds the standard
//! library and the Windows SDK on its own), so we ask the compiler.

use std::process::{Command, Stdio};

/// Parses the `#include <...> search starts here:` block of `-v` output.
pub fn parse_verbose(stderr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut on = false;
    for line in stderr.lines() {
        if line.starts_with("#include <...>") {
            on = true;
        } else if line.starts_with("End of search list") {
            break;
        } else if on && line.starts_with(' ') {
            let p = line.trim().trim_end_matches("(framework directory)").trim();
            let mut p = p.replace('\\', "/");
            if p.len() > 1 && p.ends_with('/') {
                p.pop();
            }
            if !p.is_empty() {
                out.push(p);
            }
        }
    }
    out
}

fn from_env_include() -> Vec<String> {
    std::env::var("INCLUDE")
        .map(|v| v.split(if cfg!(windows) { ';' } else { ':' }).filter(|s| !s.is_empty()).map(|s| s.replace('\\', "/")).collect())
        .unwrap_or_default()
}

/// Built-in include directories of `compiler`, best effort. Empty on failure.
pub fn discover(compiler: &str, std: Option<&str>) -> Vec<String> {
    let name = compiler.rsplit('/').next().unwrap_or(compiler).to_ascii_lowercase();
    if name == "cl.exe" || name == "cl" {
        return from_env_include();
    }
    let mut cmd = Command::new(compiler);
    cmd.args(["-E", "-x", "c++", "-v"]);
    if let Some(s) = std {
        cmd.arg(format!("-std={s}"));
    }
    cmd.arg(if cfg!(windows) { "NUL" } else { "/dev/null" });
    let Ok(out) = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output() else {
        return from_env_include();
    };
    let dirs = parse_verbose(&String::from_utf8_lossy(&out.stderr));
    if dirs.is_empty() { from_env_include() } else { dirs }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clang_output() {
        let s = "clang version 18\n#include \"...\" search starts here:\n#include <...> search starts here:\n C:\\Program Files\\LLVM\\lib\\clang\\18\\include\n C:\\Program Files (x86)\\Windows Kits\\10\\include\\10.0\\um/\n /sys/frameworks (framework directory)\nEnd of search list.\n";
        assert_eq!(
            parse_verbose(s),
            ["C:/Program Files/LLVM/lib/clang/18/include", "C:/Program Files (x86)/Windows Kits/10/include/10.0/um", "/sys/frameworks"]
        );
    }
}
