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
| `kcore_rpc` | Any implemented unary controller, node, kcore-provisioning, vault, or vault-install RPC. Omit `method` to list them. Provisioning needs `connection.provisioning`. Vault defaults to `127.0.0.1:9191`. Mutations need `confirm: true`. |

Node install partitions disks. It runs only after the operator sets `spec.acknowledge` to `wipe the selected disks`.

## Resources

Cluster context, VM, network, SSH key, container, volume, volume snapshot, snapshot policy, security group, disk layout, Ceph cluster, shared filesystem, object store, and cluster update.

## Cursor

This directory is a Cursor plugin. After `npm run build`, install it as a local plugin (the folder that contains `.cursor-plugin/plugin.json`). The rule and the `kcore-operator` skill tell the agent to ask, plan, then apply.

The GitHub repository is a marketplace of one plugin. `.cursor-plugin/marketplace.json` at the repository root points at this directory. Submit that repository at [cursor.com/marketplace/publish](https://cursor.com/marketplace/publish). The plugin name is `kcore`.

`mcp.json` starts `node ${CURSOR_PLUGIN_ROOT}/packages/server/dist/index.js`. Run `npm run build` before you tag a release so the committed bundle is current.

## VS Code

Two listings:

- The MCP server gallery (`@mcp` in the Extensions view) is GitHub's curated registry. Publish to the official MCP Registry, then email [partnerships@github.com](mailto:partnerships@github.com) and ask them to include `io.github.kcorehypervisor/kcore`. New versions sync from the official registry after that.
- `packages/vscode` is an extension that registers the same server for Copilot agent mode. Its Marketplace publisher id is `kcore`. The release workflow publishes it when `VSCE_PAT` is set, and publishes to Open VSX when `OVSX_PAT` is set.

```bash
npm run build
cd packages/vscode
npx @vscode/vsce package
```

## Other MCP clients

Claude Desktop, Continue, Zed, and other clients that read an MCP config can use the npm package:

```json
{
  "mcpServers": {
    "kcore": {
      "command": "npx",
      "args": ["-y", "kcore-mcp"]
    }
  }
}
```

From a checkout, before the package is on npm:

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

The server speaks stdio and reads `~/.kcore/config` on the machine where it runs. `KCORE_CONFIG` overrides that path. The controller call presents the context client certificate. `KCORE_PROVISIONING` sets the default provisioning address. Provisioning and the vault listen in plaintext unless `connection.provisioningInsecure` or `connection.vaultInsecure` is false (or `KCORE_VAULT_TLS=1`). `KCORE_PROVISIONING_TOKEN` and `KCORE_VAULT_TOKEN` are sent as a bearer token when those servers require one. `KCORE_VAULT_ENDPOINT` overrides the vault address.

## Publish

The official registry name is `io.github.kcorehypervisor/kcore` (`packages/server/server.json`). The npm package name is `kcore-mcp`, and `mcpName` in that package is the registry name. One tag publishes the automated listings:

```bash
git tag kcore-mcp-v0.1.0
git push origin kcore-mcp-v0.1.0
```

The tag version must be the release you want on npm. The workflow writes that version into the package manifests before it publishes. Add these Actions secrets on `kcorehypervisor/kcore` first:

| Secret | Listing |
| --- | --- |
| `NPM_TOKEN` | npm. Required. The official MCP Registry rejects the server until this package is public. Use an automation token that can publish `kcore-mcp`. |
| `VSCE_PAT` | Visual Studio Marketplace extension. Create the publisher `kcore` first. |
| `OVSX_PAT` | Open VSX extension. |

The workflow logs in to the MCP Registry with GitHub OIDC and publishes `server.json`. Glama, PulseMCP, and mcp.so import that registry on their own schedule. `smithery.yaml` at the repository root is the Smithery listing; connect the GitHub repo in the Smithery dashboard after the npm package exists. `kcore-mcp/glama.json` names the maintainer for Glama.

These listings are not automated from CI:

- Cursor Marketplace. Submit [the repository](https://github.com/kcorehypervisor/kcore) at [cursor.com/marketplace/publish](https://cursor.com/marketplace/publish).
- VS Code MCP gallery. Email [partnerships@github.com](mailto:partnerships@github.com) after the official registry shows the server.
- Claude directory and OpenAI plugins. Both accept remote HTTPS servers. This server is local stdio, because it has to read the operator's `~/.kcore` certificates.
- Cline and LobeHub. Submit the public GitHub repo from their dashboards.
- mcp.so, MCP Market, and mcpservers.org. The free path is the official registry. Each also sells a paid skip of their review queue.

## Develop

```bash
npm test
node packages/server/dist/index.js
```

The server speaks MCP over stdio. Logs stay off stdout.
