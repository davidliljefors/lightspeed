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
  ctx.subscriptions.push(vscode.commands.registerCommand("lightspeed.findText", () => findText(client!)));
}

interface TextMatch {
  uri: string;
  range: vscode.Range;
  preview: string;
}

interface FindTextResult {
  matches: { uri: string; range: { start: { line: number; character: number }; end: { line: number; character: number } }; preview: string }[];
  truncated: boolean;
  files: number;
}

interface MatchItem extends vscode.QuickPickItem {
  match: TextMatch;
}

/** Live text search over the files the server has indexed (compile database files and their includes). */
async function findText(lc: LanguageClient) {
  const qp = vscode.window.createQuickPick<MatchItem>();
  const toggles = { regex: false, caseSensitive: false, wholeWord: false };
  const buttons = [
    { key: "caseSensitive" as const, icon: "case-sensitive", tip: "Match Case" },
    { key: "wholeWord" as const, icon: "whole-word", tip: "Match Whole Word" },
    { key: "regex" as const, icon: "regex", tip: "Use Regular Expression" },
  ];
  const paintButtons = () => {
    qp.buttons = buttons.map((b) => ({
      iconPath: new vscode.ThemeIcon(b.icon),
      tooltip: `${b.tip}${toggles[b.key] ? " (on)" : ""}`,
    }));
    qp.title = "Find in indexed code" + buttons.filter((b) => toggles[b.key]).map((b) => `  [${b.tip}]`).join("");
  };
  paintButtons();
  qp.placeholder = "Search text in indexed C/C++ files";
  qp.matchOnDescription = false;
  qp.matchOnDetail = false;
  const sel = vscode.window.activeTextEditor?.selection;
  if (sel && !sel.isEmpty && sel.isSingleLine) {
    qp.value = vscode.window.activeTextEditor!.document.getText(sel);
  }

  let seq = 0;
  let timer: NodeJS.Timeout | undefined;
  const run = async () => {
    const mine = ++seq;
    const query = qp.value;
    if (!query) {
      qp.items = [];
      return;
    }
    qp.busy = true;
    try {
      const r = await lc.sendRequest<FindTextResult | null>("lightspeed/findText", { query, ...toggles, maxResults: 1000 });
      if (mine !== seq) {
        return;
      }
      const matches = r?.matches ?? [];
      qp.items = matches.map((m) => {
        const uri = vscode.Uri.parse(m.uri);
        const range = new vscode.Range(m.range.start.line, m.range.start.character, m.range.end.line, m.range.end.character);
        return {
          // `$(` would render as a codicon.
          label: m.preview.replace(/\$\(/g, "\\$("),
          description: `${vscode.workspace.asRelativePath(uri)}:${range.start.line + 1}`,
          alwaysShow: true,
          match: { uri: m.uri, range, preview: m.preview },
        };
      });
      qp.title = `Find in indexed code — ${matches.length}${r?.truncated ? "+" : ""} matches in ${r?.files ?? 0} files`;
    } catch (e) {
      if (mine === seq) {
        qp.items = [];
        qp.title = `Find in indexed code — ${(e as Error).message}`;
      }
    } finally {
      if (mine === seq) {
        qp.busy = false;
      }
    }
  };

  qp.onDidChangeValue(() => {
    clearTimeout(timer);
    timer = setTimeout(run, 120);
  });
  qp.onDidTriggerButton((b) => {
    const i = qp.buttons.indexOf(b);
    if (i >= 0) {
      toggles[buttons[i].key] = !toggles[buttons[i].key];
      paintButtons();
      void run();
    }
  });
  const reveal = async (m: TextMatch, preview: boolean) => {
    const doc = await vscode.workspace.openTextDocument(vscode.Uri.parse(m.uri));
    await vscode.window.showTextDocument(doc, { selection: m.range, preview, preserveFocus: preview });
  };
  qp.onDidChangeActive((items) => {
    if (items[0]) {
      void reveal(items[0].match, true);
    }
  });
  qp.onDidAccept(() => {
    const it = qp.selectedItems[0];
    qp.hide();
    if (it) {
      void reveal(it.match, false);
    }
  });
  qp.onDidHide(() => {
    clearTimeout(timer);
    seq++;
    qp.dispose();
  });
  qp.show();
  if (qp.value) {
    void run();
  }
}

export function deactivate() {
  return client?.stop();
}
