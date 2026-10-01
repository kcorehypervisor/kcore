import * as vscode from "vscode";
import { join } from "node:path";

export function activate(context: vscode.ExtensionContext): void {
  const didChange = new vscode.EventEmitter<void>();
  context.subscriptions.push(didChange);
  context.subscriptions.push(
    vscode.lm.registerMcpServerDefinitionProvider("kcore", {
      onDidChangeMcpServerDefinitions: didChange.event,
      provideMcpServerDefinitions: () => {
        const server = join(context.extensionPath, "dist", "mcp.js");
        return [new vscode.McpStdioServerDefinition("kcore", "node", [server], {}, "0.1.0")];
      },
    }),
  );
}

export function deactivate(): void {}
