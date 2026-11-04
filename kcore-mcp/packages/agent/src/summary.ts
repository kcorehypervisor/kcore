import { X509Certificate } from "node:crypto";
import { homedir } from "node:os";
import { join } from "node:path";
import { resolveConnection } from "../../server/src/connection.js";

export type ClusterSummary = {
  controller?: string;
  operator?: string;
  insecure?: boolean;
  error?: string;
};

export function clusterSummary(): ClusterSummary {
  try {
    const connection = resolveConnection();
    let operator: string | undefined;
    if (connection.cert) {
      const subject = new X509Certificate(connection.cert).subject;
      operator = /CN=([^\n,]+)/.exec(subject)?.[1];
    }
    return { controller: connection.address, operator, insecure: connection.insecure };
  } catch (error) {
    return { error: error instanceof Error ? error.message : String(error) };
  }
}

export function defaultHome(): string {
  return process.env.KCORE_AGENT_HOME?.trim() || join(homedir(), ".kcore", "agent");
}

