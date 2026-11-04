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
if (bind !== "127.0.0.1" && bind !== "::1") {
  console.error(`KCORE_AGENT_BIND=${bind} is not a loopback address. Use an SSH tunnel for remote access.`);
  process.exit(2);
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
});

app.server.listen(port, bind, () => {
  const address = app.server.address();
  if (address && typeof address === "object") {
    // `kctl agent` waits for this exact line.
    console.log(`listening http://127.0.0.1:${address.port}`);
  }
});

for (const signal of ["SIGINT", "SIGTERM"] as const) {
  process.on(signal, () => {
    void app.close().finally(() => process.exit(0));
  });
}
