import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const controller = process.env.KCORE_CONTROLLER_WEB ?? "https://127.0.0.1:9443";

export default defineConfig({
  plugins: [react()],
  server: {
    // The controller's web listener serves RPCs under /kcore.controller.<Service>/.
    proxy: { "^/kcore\\.controller\\.": { target: controller, changeOrigin: true, secure: false } },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      output: {
        // Only libraries that never import React get their own chunk; splitting
        // React from its dependents makes the chunks import each other in a cycle.
        manualChunks(id) {
          if (!id.includes("node_modules")) return undefined;
          if (id.includes("@xterm")) return "xterm";
          if (id.includes("@bufbuild") || id.includes("@connectrpc")) return "rpc";
          return "vendor";
        },
      },
    },
  },
});
