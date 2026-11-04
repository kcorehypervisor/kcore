import { build } from "esbuild";
import { chmodSync, cpSync, mkdirSync, rmSync } from "node:fs";

rmSync("dist", { recursive: true, force: true });
mkdirSync("dist", { recursive: true });
await build({
  entryPoints: ["src/index.ts"],
  bundle: true,
  platform: "node",
  format: "esm",
  outfile: "dist/index.js",
  banner: {
    js: '#!/usr/bin/env node\nimport { createRequire as __kcoreCreateRequire } from "node:module"; const require = __kcoreCreateRequire(import.meta.url);',
  },
});
chmodSync("dist/index.js", 0o755);
cpSync("ui", "dist/ui", { recursive: true });
cpSync("../server/proto", "dist/proto", { recursive: true });
