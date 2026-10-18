import { readFileSync } from "node:fs";
import { join } from "node:path";
import * as vscode from "vscode";

export function activate(context: vscode.ExtensionContext): void {
  const didChange = new vscode.EventEmitter<void>();
  context.subscriptions.push(didChange);
  context.subscriptions.push(
    vscode.lm.registerMcpServerDefinitionProvider("kcore", {
      onDidChangeMcpServerDefinitions: didChange.event,
      provideMcpServerDefinitions: () => {
        const server = join(context.extensionPath, "dist", "mcp.js");
        const version = JSON.parse(readFileSync(join(context.extensionPath, "package.json"), "utf8")).version as string;
        return [new vscode.McpStdioServerDefinition("kcore", "node", [server], {}, version)];
      },
    }),
  );
}

export function deactivate(): void {}
