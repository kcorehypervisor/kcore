import { join } from "node:path";
import * as grpc from "@grpc/grpc-js";
import * as protoLoader from "@grpc/proto-loader";
import { parseAllDocuments } from "yaml";
import { protoDir } from "./grpc.js";

export type VaultKind = "BackupPolicy" | "BackupTarget" | "Replication";

type VaultCtor = new (address: string, creds: grpc.ChannelCredentials) => grpc.Client;

let ctor: VaultCtor | undefined;

function vaultCtor(): VaultCtor {
  if (ctor) return ctor;
  const dir = protoDir();
  const definition = protoLoader.loadSync(join(dir, "vault.proto"), {
    keepCase: false,
    longs: String,
    enums: String,
    defaults: true,
    oneofs: true,
    includeDirs: [dir],
  });
  const pkg = grpc.loadPackageDefinition(definition) as unknown as {
    kcore: { vault: { Vault: VaultCtor } };
  };
  ctor = pkg.kcore.vault.Vault;
  return ctor;
}

export function vaultAddress(explicit?: string): string {
  const raw = (explicit || process.env.KCORE_VAULT_ENDPOINT || "127.0.0.1:9191").trim();
  return raw.replace(/^https?:\/\//, "");
}

export function unconfirmed(confirm: boolean | undefined): string | undefined {
  if (confirm === true) return undefined;
  return "confirm must be true";
}

function call(address: string, method: string, request: Record<string, unknown>): Promise<unknown> {
  const Client = vaultCtor();
  const client = new Client(address, grpc.credentials.createInsecure()) as grpc.Client & Record<string, unknown>;
  const fn = client[method];
  if (typeof fn !== "function") {
    client.close();
    return Promise.reject(new Error(`vault method ${method} is not available`));
  }
  return new Promise((resolve, reject) => {
    (fn as (req: unknown, cb: (err: Error | null, resp: unknown) => void) => void).call(
      client,
      request,
      (err, resp) => {
        client.close();
        if (err) reject(err);
        else resolve(resp);
      },
    );
  });
}

export function documents(text: string): Array<Record<string, unknown>> {
  return parseAllDocuments(text)
    .map((doc) => doc.toJSON() as Record<string, unknown> | null)
    .filter((doc): doc is Record<string, unknown> => !!doc && typeof doc === "object");
}

function keepOf(spec: Record<string, unknown>): Record<string, number> {
  const keep = (spec.keep ?? {}) as Record<string, unknown>;
  const num = (key: string) => Number(keep[key] ?? spec[`keep_${key}`] ?? 0) || 0;
  return {
    hourly: num("hourly"),
    daily: num("daily"),
    weekly: num("weekly"),
    monthly: num("monthly"),
    yearly: num("yearly"),
  };
}

function asStrings(value: unknown): string[] {
  if (Array.isArray(value)) return value.map((item) => String(item));
  if (typeof value === "string" && value.trim()) return [value.trim()];
  return [];
}

export async function readVault(address: string, view: string): Promise<unknown> {
  if (view === "jobs") return call(address, "listBackupJobs", {});
  if (view === "catalog") return call(address, "listBackups", {});
  return call(address, "listInventory", {});
}

export async function applyVault(address: string, spec: Record<string, unknown>): Promise<unknown> {
  const kind = String(spec.kind ?? "");
  if (kind === "BackupTarget") {
    return call(address, "applyTarget", {
      target: {
        name: String(spec.name ?? ""),
        backend: String(spec.backend ?? "fs"),
        location: String(spec.location ?? ""),
        disableObjectLock: Boolean(spec.disable_object_lock),
      },
    });
  }
  if (kind === "BackupPolicy") {
    const keep = keepOf(spec);
    const names = [
      ...asStrings(spec.target),
      ...asStrings(spec.targets),
      ...asStrings(spec.secondary),
    ];
    return call(address, "applyBackupPolicy", {
      policy: {
        name: String(spec.name ?? ""),
        schedule: String(spec.schedule ?? "@daily"),
        consistency: "CONSISTENCY_LEVEL_FS",
        selectorVmNames: asStrings(spec.vms),
        targets: names.map((targetName) => ({ targetName, keep })),
        enabled: spec.enabled !== false,
        preHook: String(spec.pre_hook ?? ""),
        postHook: String(spec.post_hook ?? ""),
      },
    });
  }
  if (kind === "Replication") {
    return call(address, "createReplicationJob", {
      sourceTarget: String(spec.source ?? spec.source_target ?? ""),
      destTarget: String(spec.dest ?? spec.dest_target ?? ""),
    });
  }
  throw new Error(`unknown vault kind ${kind}`);
}

export async function operateVault(address: string, spec: Record<string, unknown>): Promise<unknown> {
  const action = String(spec.action ?? "");
  if (action === "backup") {
    return call(address, "createBackupJob", {
      vmName: String(spec.vm ?? ""),
      targetName: String(spec.target ?? ""),
      consistency: String(spec.consistency ?? "CONSISTENCY_LEVEL_FS"),
      forceFull: Boolean(spec.force_full),
    });
  }
  if (action === "restore") {
    return call(address, "createRestoreJob", {
      backupId: String(spec.backup ?? ""),
      asVmName: String(spec.as_vm_name ?? ""),
      destPath: String(spec.dest ?? ""),
      volumeName: String(spec.volume ?? ""),
      node: String(spec.node ?? ""),
      instant: Boolean(spec.instant),
    });
  }
  if (action === "verify") {
    return call(address, "createVerifyJob", {
      targetName: String(spec.target ?? ""),
      full: Boolean(spec.full),
      bootTest: spec.boot_test !== false,
    });
  }
  if (action === "replicate") {
    return call(address, "createReplicationJob", {
      sourceTarget: String(spec.source ?? ""),
      destTarget: String(spec.dest ?? ""),
    });
  }
  throw new Error(`unknown vault action ${action}`);
}
