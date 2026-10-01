import assert from "node:assert/strict";
import { X509Certificate } from "node:crypto";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { InputError } from "../src/names.js";
import { createClusterContext } from "../src/pki.js";

test("creating a cluster writes a CA, a controller certificate, and a client certificate", async () => {
  const root = await mkdtemp(join(tmpdir(), "kcore-pki-"));
  const config = join(root, "config");
  const certs = join(root, "certs");
  try {
    const created = await createClusterContext(
      { controller: "10.0.0.10:9090", name: "lab", certsDir: certs },
      { config },
    );
    assert.equal(created.context, "lab");
    assert.equal(created.certsDir, certs);
    const client = new X509Certificate(await readFile(join(created.certsDir, "kctl.crt")));
    const ca = new X509Certificate(await readFile(join(created.certsDir, "ca.crt")));
    const controller = new X509Certificate(await readFile(join(created.certsDir, "controller.crt")));
    assert.match(client.subject, /kctl/);
    assert.equal(client.checkIssued(ca), true);
    assert.equal(client.ca, false);
    assert.match(controller.subject, /kcore-controller-10\.0\.0\.10/);
    assert.match(controller.subjectAltName ?? "", /10\.0\.0\.10/);
    const keyMode = (await stat(join(created.certsDir, "kctl.key"))).mode & 0o777;
    assert.equal(keyMode, 0o600);
    const saved = await readFile(config, "utf8");
    assert.match(saved, /current-context: lab/);
    assert.match(saved, /cert-data:/);
    assert.doesNotMatch(saved, /BEGIN PRIVATE KEY/);
    await assert.rejects(
      () => createClusterContext({ controller: "10.0.0.10:9090", name: "lab", certsDir: certs }, { config }),
      InputError,
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
