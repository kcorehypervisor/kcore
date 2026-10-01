import { spawn } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { InputError, assertAddress, assertArg, assertName } from "./names.js";

export type KctlConnection = {
  bin?: string;
  config?: string;
  controller?: string;
  insecure?: boolean;
  tlsServerName?: string;
  operator?: string;
  node?: string;
  timeoutMs?: number;
};

export type KctlResult = {
  code: number;
  stdout: string;
  stderr: string;
  argv: string[];
};

export function connectionArgs(options: KctlConnection): string[] {
  const args: string[] = [];
  if (options.config) args.push("-c", assertArg(options.config, "config path"));
  if (options.controller) args.push("-s", assertAddress(options.controller, "controller"));
  if (options.insecure) args.push("-k");
  if (options.tlsServerName) args.push("--tls-server-name", assertName(options.tlsServerName, "tls server name"));
  if (options.operator) args.push("--as", assertName(options.operator, "operator"));
  if (options.node) args.push("--node", assertAddress(options.node, "node"));
  return args;
}

export async function runKctl(command: string[], options: KctlConnection = {}): Promise<KctlResult> {
  const bin = options.bin?.trim() || process.env.KCTL_BIN?.trim() || "kctl";
  if (bin.includes("\0") || /[\r\n]/.test(bin)) {
    throw new InputError("kctl path is invalid");
  }
  const argv = [...connectionArgs(options), ...command];
  const timeoutMs = options.timeoutMs ?? 120_000;
  return new Promise((resolve, reject) => {
    const child = spawn(bin, argv, { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(() => {
      child.kill("SIGTERM");
      reject(new InputError(`kctl timed out after ${timeoutMs}ms`));
    }, timeoutMs);
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      stdout = clip(stdout + chunk);
    });
    child.stderr.on("data", (chunk: string) => {
      stderr = clip(stderr + chunk);
    });
    child.on("error", (error: NodeJS.ErrnoException) => {
      clearTimeout(timer);
      if (error.code === "ENOENT") {
        reject(new InputError("kctl was not found on PATH. Install kctl or set KCTL_BIN."));
        return;
      }
      reject(error);
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ code: code ?? 1, stdout, stderr, argv: [bin, ...argv] });
    });
  });
}

function clip(text: string): string {
  const max = 200_000;
  return text.length > max ? text.slice(0, max) + "\n… output truncated" : text;
}

export async function runWithManifest(
  command: string[],
  fileArgIndex: number,
  yaml: string,
  options: KctlConnection,
): Promise<KctlResult> {
  const dir = await mkdtemp(join(tmpdir(), "kcore-mcp-"));
  const file = join(dir, "manifest.yaml");
  const args = command.slice();
  args[fileArgIndex] = file;
  try {
    await writeFile(file, yaml, { mode: 0o600 });
    return await runKctl(args, options);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}
