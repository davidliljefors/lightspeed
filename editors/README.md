# Editor integrations

Each editor just launches `lightspeed lsp` and talks LSP over stdio. Put `lightspeed` on `PATH` (or point the editor at it), and keep a `compile_commands.json` in the workspace. Disable the editor's other C/C++ language server (clangd, Microsoft C/C++) so results aren't duplicated.

## VS Code

```
cd editors/vscode
npm install
npm run compile
npm run package        # writes lightspeed-0.1.0.vsix
code --install-extension lightspeed-0.1.0.vsix
```

Release builds bundle the binary under `bin/`. Settings: `lightspeed.serverPath`, `lightspeed.compileCommands`.

## Zed

Zed → Extensions → *Install Dev Extension*, then pick `editors/zed`. It needs a Rust toolchain with the `wasm32-wasip2` target and finds `lightspeed` on `PATH`. In `settings.json`:

```json
{ "languages": { "C++": { "language_servers": ["lightspeed", "!clangd", "..."] } } }
```

## Sublime Text

Install the **LSP** package, then copy `editors/sublime` to `Packages/LSP-lightspeed`. Restart Sublime or run *LSP: Enable Language Server Globally*.
