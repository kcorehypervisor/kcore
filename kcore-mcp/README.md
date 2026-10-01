# kcore MCP

MCP server and editor plugins for [kcore](https://kcorehypervisor.com). An agent can create a cluster (CA, controller certificate, and client certificate), VMs, networks, volumes, Ceph resources, and the rest of the declarative controller API. The same upsert rules Terraform and Crossplane use apply here: create updates mutable fields and rejects immutable ones.

The server asks before it changes anything. `kcore_advise` returns the questions an operator still has to answer. Plan and apply refuse to run while those questions are open. Clients that support MCP elicitation also get a form for the missing answers.

## What you need

- Node.js 20 or newer
- A context in `~/.kcore/config` for anything except the first cluster bootstrap, which this server creates (certificates included)

```bash
npm install
npm test
npm run build
```

## Tools

| Tool | Role |
| --- | --- |
| `kcore_advise` | Questions to ask before a change. Read-only. |
| `kcore_plan` | Local diff, plus a controller read when `checkCluster` is set. Cluster plans describe the certificates that will be written. |
| `kcore_apply` | Declarative apply after `confirm: true`. Cluster apply writes the CA, controller certificate, and client certificate into `~/.kcore`. |
| `kcore_read` | Controller get and describe RPCs. |
| `kcore_delete` | Delete, or cancel a non-terminal cluster update. |
| `kcore_operation` | Power state, migrate, drain, cordon, node approval, update approve/rollback, node install. |
| `kcore_catalog` | Resource kinds and which fields are mutable. |
| `kcore_rpc` | Any other implemented unary controller or node RPC. Omit `method` to list them. Mutations need `confirm: true`. |

Node install partitions disks. It runs only after the operator sets `spec.acknowledge` to `wipe the selected disks`.

## Resources

Cluster context, VM, network, SSH key, container, volume, volume snapshot, snapshot policy, security group, disk layout, Ceph cluster, shared filesystem, object store, and cluster update.

## Cursor

This repository is a Cursor plugin. After `npm run build`, install it as a local plugin (the folder that contains `.cursor-plugin/plugin.json`). The rule and the `kcore-operator` skill tell the agent to ask, plan, then apply.

To publish it so other engineers can find kcore:

1. Push this repository to a public git host.
2. Open [cursor.com/marketplace/publish](https://cursor.com/marketplace/publish) and submit that repository.
3. Keep `name` as `kcore` unless the marketplace already has that name.

`mcp.json` starts `node ./packages/server/dist/index.js`, so the published tree needs the built server. Run `npm run build` before you tag a release.

## VS Code

`packages/vscode` is an extension that registers the same MCP server for Copilot agent mode and any other VS Code client that loads MCP providers.

```bash
npm run build
cd packages/vscode
npx @vscode/vsce package
```

Set `publisher` in `packages/vscode/package.json` to your Marketplace publisher before `vsce publish`. The placeholder publisher is `kcore`.

## Other MCP clients

Claude Desktop, Continue, Zed, and other clients that read `mcp.json` can use the same server:

```json
{
  "mcpServers": {
    "kcore": {
      "command": "node",
      "args": ["/absolute/path/to/kcore-mcp/packages/server/dist/index.js"]
    }
  }
}
```

After the package is on npm, `npx -y kcore-mcp` is enough. The npm name is `kcore-mcp`.

## Develop

```bash
npm test
node packages/server/dist/index.js
```

The server speaks MCP over stdio. Logs stay off stdout.
