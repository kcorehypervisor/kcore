import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { InputError } from "../src/names.js";
import { resolveConnection, resolveProvisioning, resolveVault } from "../src/connection.js";

const PEM = Buffer.from("-----BEGIN TEST-----\nYQ==\n-----END TEST-----\n", "utf8").toString("base64");

function config(extra = ""): string {
  const dir = mkdtempSync(join(tmpdir(), "kcore-mcp-"));
  const path = join(dir, "config");
  writeFileSync(
    path,
    `current-context: lab
contexts:
  lab:
    controller: 127.0.0.1:9090
    ca-data: ${PEM}
    cert-data: ${PEM}
    key-data: ${PEM}
${extra}`,
  );
  return path;
}

test("a context certificate must be base64-encoded PEM", () => {
  const dir = mkdtempSync(join(tmpdir(), "kcore-mcp-bad-"));
  const bad = join(dir, "config");
  writeFileSync(
    bad,
    `current-context: lab
contexts:
  lab:
    controller: 127.0.0.1:9090
    ca-data: ${Buffer.from("not-a-pem", "utf8").toString("base64")}
    cert-data: ${PEM}
    key-data: ${PEM}
`,
  );
  assert.throws(() => resolveConnection({ config: bad }), /not a PEM/);
});

test("provisioning sends a bearer token and stays plaintext unless asked for the client certificate", () => {
  const previous = process.env.KCORE_PROVISIONING_TOKEN;
  process.env.KCORE_PROVISIONING_TOKEN = "lab-token";
  try {
    const plain = resolveProvisioning({ provisioning: "127.0.0.1:9091", config: join(tmpdir(), "missing-kcore-config") });
    assert.equal(plain.insecure, true);
    assert.equal(plain.authorization, "Bearer lab-token");
    assert.equal(plain.cert, undefined);

    const tls = resolveProvisioning({
      provisioning: "127.0.0.1:9091",
      provisioningInsecure: false,
      config: config("    provisioning: 10.0.0.8:9091\n"),
      token: "Bearer already",
    });
    assert.equal(tls.insecure, false);
    assert.equal(tls.address, "127.0.0.1:9091");
    assert.equal(tls.authorization, "Bearer already");
    assert.ok(tls.cert?.toString("utf8").includes("-----BEGIN"));
  } finally {
    if (previous === undefined) delete process.env.KCORE_PROVISIONING_TOKEN;
    else process.env.KCORE_PROVISIONING_TOKEN = previous;
  }
});

test("the vault defaults to plaintext on 127.0.0.1:9191 and can present the kcore client certificate", () => {
  const previousTls = process.env.KCORE_VAULT_TLS;
  const previousEndpoint = process.env.KCORE_VAULT_ENDPOINT;
  const previousToken = process.env.KCORE_VAULT_TOKEN;
  delete process.env.KCORE_VAULT_TLS;
  delete process.env.KCORE_VAULT_ENDPOINT;
  delete process.env.KCORE_VAULT_TOKEN;
  try {
    const plain = resolveVault({ config: join(tmpdir(), "missing-kcore-config") });
    assert.equal(plain.address, "127.0.0.1:9191");
    assert.equal(plain.insecure, true);
    assert.equal(plain.authorization, undefined);

    process.env.KCORE_VAULT_TLS = "1";
    const tls = resolveVault({ config: config("    vault: 10.1.0.9:9191\n"), token: "vault-token" });
    assert.equal(tls.address, "10.1.0.9:9191");
    assert.equal(tls.insecure, false);
    assert.equal(tls.authorization, "Bearer vault-token");
    assert.match(tls.key?.toString("utf8") ?? "", /-----BEGIN/);
  } finally {
    if (previousTls === undefined) delete process.env.KCORE_VAULT_TLS;
    else process.env.KCORE_VAULT_TLS = previousTls;
    if (previousEndpoint === undefined) delete process.env.KCORE_VAULT_ENDPOINT;
    else process.env.KCORE_VAULT_ENDPOINT = previousEndpoint;
    if (previousToken === undefined) delete process.env.KCORE_VAULT_TOKEN;
    else process.env.KCORE_VAULT_TOKEN = previousToken;
  }
});

test("provisioning without an address explains what is missing", () => {
  const previous = process.env.KCORE_PROVISIONING;
  delete process.env.KCORE_PROVISIONING;
  try {
    assert.throws(
      () => resolveProvisioning({ config: join(tmpdir(), "missing-kcore-config") }),
      (error: unknown) => error instanceof InputError && /connection.provisioning/.test(error.message),
    );
  } finally {
    if (previous === undefined) delete process.env.KCORE_PROVISIONING;
    else process.env.KCORE_PROVISIONING = previous;
  }
});
