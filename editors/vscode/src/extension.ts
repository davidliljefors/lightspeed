import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { LanguageClient, LanguageClientOptions, ServerOptions } from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export async function activate(ctx: vscode.ExtensionContext) {
  const cfg = vscode.workspace.getConfiguration("lightspeed");
  const exe = process.platform === "win32" ? "lightspeed.exe" : "lightspeed";
  const bundled = ctx.asAbsolutePath(path.join("bin", exe));
  const command = cfg.get<string>("serverPath") || (fs.existsSync(bundled) ? bundled : "lightspeed");
  const args = ["lsp"];
  const cdb = cfg.get<string>("compileCommands");
  if (cdb) {
    args.push("--cdb", cdb);
  }

  const serverOptions: ServerOptions = { command, args };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ language: "c" }, { language: "cpp" }],
  };
  client = new LanguageClient("lightspeed", "lightspeed", serverOptions, clientOptions);
  await client.start();
}

export function deactivate() {
  return client?.stop();
}
