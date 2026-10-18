# kcore

Operate [kcore](https://kcorehypervisor.com) clusters and VMs from VS Code. The extension registers the kcore MCP server for Copilot agent mode and any other client in this editor that loads MCP providers.

The agent calls `kcore_advise` before it changes anything, shows `kcore_plan`, and calls `kcore_apply` only after you agree.

## Requirements

- VS Code 1.101 or newer
- Node.js 20 or newer on `PATH`

Cluster calls, other than the first bootstrap, use the context in `~/.kcore/config`. Set `KCORE_CONFIG` to point somewhere else. Bootstrap writes the CA, controller certificate, and client certificate into that directory.

## Install from a checkout

```bash
npm install
npm run build
cd packages/vscode
npx @vscode/vsce package
```

Install the `.vsix` with **Extensions: Install from VSIX**.
