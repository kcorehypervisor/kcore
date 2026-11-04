import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { AuthStore } from "./auth.js";
import { createApp } from "./http.js";
import { createServer } from "../../server/src/server.js";
import { clusterSummary, defaultHome } from "./summary.js";

const token = process.env.KCORE_AGENT_TOKEN?.trim() ?? "";
if (token.length < 32) {
  console.error("KCORE_AGENT_TOKEN must be set to at least 32 characters. Start this with `kctl agent`.");
  process.exit(2);
}

const bind = process.env.KCORE_AGENT_BIND?.trim() || "127.0.0.1";
const loopback = bind === "127.0.0.1" || bind === "::1";
if (!loopback) {
  const specificIpv4 = /^(\d{1,3})(\.\d{1,3}){3}$/.test(bind) && bind !== "0.0.0.0";
  if (process.env.KCORE_AGENT_ALLOW_REMOTE !== "1" || !specificIpv4) {
    console.error(
      `KCORE_AGENT_BIND=${bind} is refused. Binding off loopback needs KCORE_AGENT_ALLOW_REMOTE=1 and one specific IPv4 address (not 0.0.0.0). Use \`kctl agent --listen <ip>\`, or an SSH tunnel.`,
    );
    process.exit(2);
  }
}
const port = Number(process.env.KCORE_AGENT_PORT ?? "0");
if (!Number.isInteger(port) || port < 0 || port > 65535) {
  console.error(`KCORE_AGENT_PORT=${process.env.KCORE_AGENT_PORT} is not a port number.`);
  process.exit(2);
}

const home = defaultHome();
const here = dirname(fileURLToPath(import.meta.url));
const app = createApp({
  token,
  auth: new AuthStore(home),
  createServer,
  uiDir: process.env.KCORE_AGENT_UI_DIR?.trim() || join(here, "ui"),
  logDir: join(home, "sessions"),
  summary: clusterSummary,
  allowedHosts: loopback ? [] : [bind],
});

app.server.listen(port, bind, () => {
  const address = app.server.address();
  if (address && typeof address === "object") {
    // `kctl agent` waits for this exact line.
    console.log(`listening http://${bind}:${address.port}`);
  }
});

for (const signal of ["SIGINT", "SIGTERM"] as const) {
  process.on(signal, () => {
    void app.close().finally(() => process.exit(0));
  });
}
