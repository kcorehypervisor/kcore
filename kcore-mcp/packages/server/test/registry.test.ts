import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const serverDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const mcpRoot = join(serverDir, "..", "..");
const repoRoot = join(mcpRoot, "..");

test("server.json matches the npm package the official MCP registry verifies", () => {
  const pkg = JSON.parse(readFileSync(join(serverDir, "package.json"), "utf8"));
  const server = JSON.parse(readFileSync(join(serverDir, "server.json"), "utf8"));

  assert.equal(server.name, "io.github.kcorehypervisor/kcore");
  assert.equal(pkg.mcpName, server.name);
  assert.equal(server.version, pkg.version);
  assert.ok(server.description.length > 0 && server.description.length <= 100);
  assert.equal(server.packages.length, 1);
  assert.equal(server.packages[0].registryType, "npm");
  assert.equal(server.packages[0].identifier, pkg.name);
  assert.equal(server.packages[0].version, pkg.version);
  assert.equal(server.packages[0].transport.type, "stdio");
  assert.match(pkg.scripts.build, /dist\/proto/);
  assert.equal(pkg.publishConfig.access, "public");
});

test("the Cursor plugin manifest points at this repository", () => {
  const plugin = JSON.parse(readFileSync(join(mcpRoot, ".cursor-plugin", "plugin.json"), "utf8"));
  const marketplace = JSON.parse(readFileSync(join(repoRoot, ".cursor-plugin", "marketplace.json"), "utf8"));
  const mcp = JSON.parse(readFileSync(join(mcpRoot, "mcp.json"), "utf8"));

  assert.equal(plugin.name, "kcore");
  assert.equal(marketplace.plugins.length, 1);
  assert.equal(marketplace.plugins[0].name, "kcore");
  assert.equal(marketplace.plugins[0].source, "kcore-mcp");
  assert.equal(mcp.mcpServers.kcore.command, "node");
  assert.deepEqual(mcp.mcpServers.kcore.args, ["${CURSOR_PLUGIN_ROOT}/packages/server/dist/index.js"]);
});
