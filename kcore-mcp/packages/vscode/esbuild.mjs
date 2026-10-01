import * as esbuild from "esbuild";
import { cp, copyFile, mkdir } from "node:fs/promises";

await esbuild.build({
  entryPoints: ["src/extension.ts"],
  outfile: "dist/extension.js",
  bundle: true,
  platform: "node",
  format: "cjs",
  external: ["vscode"],
  sourcemap: true,
});

await mkdir("dist", { recursive: true });
await copyFile("../server/dist/index.js", "dist/mcp.js");
await cp("../server/proto", "dist/proto", { recursive: true });
