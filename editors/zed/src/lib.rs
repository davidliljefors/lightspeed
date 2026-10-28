use zed_extension_api::{self as zed, LanguageServerId, Result};

struct Lightspeed;

impl zed::Extension for Lightspeed {
    fn new() -> Self {
        Self
    }

    fn language_server_command(&mut self, _id: &LanguageServerId, worktree: &zed::Worktree) -> Result<zed::Command> {
        let command = worktree.which("lightspeed").ok_or("`lightspeed` was not found on PATH")?;
        Ok(zed::Command { command, args: vec!["lsp".into()], env: Vec::new() })
    }
}

zed::register_extension!(Lightspeed);
