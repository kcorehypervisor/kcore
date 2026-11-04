import { create, type DescMessage, type DescService, type Message } from "@bufbuild/protobuf";
import { createRouterTransport, type ConnectRouter } from "@connectrpc/connect";
import { beforeAll, describe, expect, it } from "vitest";
import { RESOURCES } from "./catalog";
import { PRESETS } from "./columns";
import { bytes, FormError, soak } from "./forms";
import { Auth, Controller, ControllerAdmin } from "./gen/controller_pb";
import { setTransport } from "./rpc";

/** A message with every nested message set and one element in each repeated one, so row mapping runs. */
function sample(desc: DescMessage, depth = 0): Message {
  const init: Record<string, unknown> = {};
  if (depth < 4) {
    for (const field of desc.fields) {
      if (field.oneof) continue;
      if (field.fieldKind === "message") init[field.localName] = sample(field.message, depth + 1);
      if (field.fieldKind === "list" && field.listKind === "message") init[field.localName] = [sample(field.message, depth + 1)];
    }
  }
  return create(desc, init as never);
}

function serveUnary(router: ConnectRouter, service: DescService, succeed = false) {
  const impl: Record<string, () => Message> = {};
  for (const method of service.methods) {
    if (method.methodKind !== "unary") continue;
    impl[method.localName] = () => {
      const reply = sample(method.output) as Record<string, unknown>;
      if (succeed && method.output.fields.some((field) => field.localName === "success")) reply.success = true;
      return reply as unknown as Message;
    };
  }
  router.service(service, impl as never);
}

beforeAll(() => {
  setTransport(
    createRouterTransport((router) => {
      serveUnary(router, Controller);
      serveUnary(router, ControllerAdmin);
      serveUnary(router, Auth);
    }),
  );
});

describe("catalog", () => {
  it("has unique resource ids", () => {
    const ids = RESOURCES.map((resource) => resource.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  for (const resource of RESOURCES) {
    if (!resource.list) continue;
    it(`lists ${resource.id}`, async () => {
      const rows = await resource.list!({});
      expect(Array.isArray(rows)).toBe(true);
      if (resource.single) {
        expect(rows).toHaveLength(1);
        return;
      }
      for (const row of rows) expect(row).toHaveProperty(resource.idField);
    });
  }

  for (const resource of RESOURCES) {
    const preset = PRESETS[resource.id];
    if (!preset || !resource.list) continue;
    it(`has ${resource.id} columns that exist in its rows`, async () => {
      const rows = await resource.list!({});
      const keys = new Set(rows.flatMap((row) => Object.keys(row)));
      expect(preset.map((column) => column.key).filter((key) => !keys.has(key))).toEqual([]);
    });
  }

  for (const resource of RESOURCES) {
    if (!resource.inspect) continue;
    it(`inspects ${resource.id}`, async () => {
      const rows = (await resource.list?.({})) ?? [];
      const key = String(rows[0]?.[resource.idField] ?? "sample");
      const detail = await resource.inspect!(key);
      expect(detail.detail).toBeDefined();
    });
  }
});

describe("bytes", () => {
  it.each([
    ["4294967296", 4294967296n],
    ["4G", 4n << 30n],
    ["4 GiB", 4n << 30n],
    ["512 MB", 512n << 20n],
    ["1.5 TiB", 3n << 39n],
    ["", 0n],
  ])("reads %s", (input, expected) => {
    expect(bytes({ size: input }, "size")).toBe(expected);
  });

  it.each(["4 GB RAM", "-1", "0.5 B"])("refuses %s", (input) => {
    expect(() => bytes({ size: input }, "size")).toThrow(FormError);
  });
});

describe("Slurm and vGPU controls", () => {
  beforeAll(() => {
    setTransport(createRouterTransport((router) => serveUnary(router, Controller, true)));
  });

  const resource = (id: string) => RESOURCES.find((candidate) => candidate.id === id)!;
  const action = (id: string, name: string) => resource(id).actions.find((candidate) => candidate.name === name)!;

  it("offers plan, node drain and resume on Slurm clusters", () => {
    const names = resource("slurm-cluster").actions.map((candidate) => candidate.name);
    expect(names).toEqual(expect.arrayContaining(["plan", "drain-node", "resume-node", "rotate-munge-key", "delete"]));
    expect(resource("slurm-cluster").create).toBeDefined();
    expect(resource("slurm-cluster").checks).toEqual(["plan"]);
  });

  it("creates and plans a Slurm cluster from a JSON spec", async () => {
    const spec = JSON.stringify({
      clusterName: "hpc-a",
      controller: { nodeId: "node-1", address: "10.0.0.1", iface: "enp6s0" },
      compute: [{ nodeId: "node-2", address: "10.0.0.2", iface: "enp6s0", partitions: ["batch"] }],
      partitions: [{ name: "batch", default: true }],
    });
    await expect(resource("slurm-cluster").create!({ name: "hpc-a", spec })).resolves.toMatchObject({ ok: true });
    await expect(action("slurm-cluster", "plan").run({ name: "hpc-a", spec })).resolves.toBeDefined();
  });

  it("refuses a Slurm spec that is not a Slurm cluster", async () => {
    await expect(resource("slurm-cluster").create!({ name: "x", spec: '{"nope": 1}' })).rejects.toThrow(FormError);
    await expect(resource("slurm-cluster").create!({ name: "x", spec: "[]" })).rejects.toThrow(FormError);
  });

  it("drains and resumes a Slurm node", async () => {
    await expect(action("slurm-cluster", "drain-node").run({ name: "hpc-a", node_id: "node-2", reason: "maintenance" })).resolves.toEqual({ ok: true });
    await expect(action("slurm-cluster", "resume-node").run({ name: "hpc-a", node_id: "node-2" })).resolves.toEqual({ ok: true });
    await expect(action("slurm-cluster", "drain-node").run({ name: "hpc-a" })).rejects.toThrow(FormError);
  });

  it("enables a vGPU host from a node and demands the manager only for NVIDIA", async () => {
    const enable = action("nodes", "vgpu-enable");
    expect(enable.confirm).toBe(true);
    await expect(enable.run({ node_id: "node-1", vendor: "intel", host_cards: "0000:03:00.0", num_vfs: "2" })).resolves.toMatchObject({ ok: true });
    await expect(enable.run({ node_id: "node-1", vendor: "nvidia", host_cards: "0000:61:00.0" })).rejects.toThrow(FormError);
    await expect(action("nodes", "vgpu-disable").run({ node_id: "node-1" })).resolves.toMatchObject({ ok: true });
  });

  it("reboots a node", async () => {
    await expect(action("nodes", "reboot").run({ node_id: "node-1", force: "true" })).resolves.toMatchObject({ ok: true });
  });

  it("names the Terraform resources that now exist", () => {
    expect(resource("ingress-gateways").terraform).toEqual(["kcore_ingress_gateway"]);
    expect(resource("ingress-routes").terraform).toEqual(["kcore_ingress_route"]);
    expect(resource("waf-policies").terraform).toEqual(["kcore_waf_policy"]);
    expect(resource("gpus").terraform).toContain("kcore_vgpu_host");
  });
});

describe("release controls", () => {
  beforeAll(() => {
    setTransport(createRouterTransport((router) => serveUnary(router, Controller, true)));
  });

  const resource = (id: string) => RESOURCES.find((candidate) => candidate.id === id)!;
  const action = (id: string, name: string) => resource(id).actions.find((candidate) => candidate.name === name)!;

  it("sets a version with a canary soak, or with no canary", async () => {
    const set = action("release", "set-version");
    expect(set.confirm).toBe(true);
    await expect(set.run({ version: "0.7.0", soak: "4h", canary_node: "node-2" })).resolves.toMatchObject({ ok: true });
    await expect(set.run({ version: "v0.7.0", no_canary: "true" })).resolves.toMatchObject({ ok: true });
    await expect(set.run({ version: "v0.7.0", no_canary: "true", soak: "1h" })).rejects.toThrow(FormError);
    await expect(set.run({ version: "v0.7.0", soak: "8d" })).rejects.toThrow(FormError);
    expect(resource("release").terraform).toEqual(["kcore_cluster_version"]);
  });

  it("promotes the canary of a cluster update, and refuses with no rollout", async () => {
    await expect(action("release", "promote").run({})).rejects.toThrow(/No version update/);
    await expect(action("cluster-updates", "promote").run({ name: "version-v0-7-0" })).resolves.toMatchObject({ ok: true });
  });

  it("creates a canary cluster update and refuses canary fields without it", async () => {
    const base = { name: "release-0-7-0", version: "0.7.0", flake_ref: "github:kcorehypervisor/kcore", all_nodes: "true" };
    await expect(resource("cluster-updates").create!({ ...base, strategy: "canary", canary_soak: "30m", canary_node: "node-2" })).resolves.toMatchObject({ ok: true });
    await expect(resource("cluster-updates").create!({ ...base, canary_node: "node-2" })).rejects.toThrow(FormError);
  });
});

describe("soak", () => {
  it.each([
    ["", 0],
    ["5400", 5400],
    ["90s", 90],
    ["45m", 2700],
    ["2h", 7200],
    ["1d", 86400],
  ])("reads %s", (input, expected) => {
    expect(soak({ soak: input }, "soak")).toBe(expected);
  });

  it.each(["0", "soon", "8d", "-1h"])("refuses %s", (input) => {
    expect(() => soak({ soak: input }, "soak")).toThrow(FormError);
  });
});

describe("ingress controls", () => {
  const resource = (id: string) => RESOURCES.find((candidate) => candidate.id === id)!;
  const action = (id: string, name: string) => resource(id).actions.find((candidate) => candidate.name === name)!;

  it("plans each ingress document before it is applied", async () => {
    for (const id of ["ingress-gateways", "ingress-routes", "waf-policies"]) expect(resource(id).checks).toEqual(["plan"]);
    await expect(action("ingress-gateways", "plan").run({ name: "edge", spec: '{"nodes":["node-1"]}' })).resolves.toBeDefined();
    await expect(action("ingress-routes", "plan").run({ name: "shop", spec: '{"gateway":"edge","hosts":["a.example.com"]}' })).resolves.toBeDefined();
    await expect(action("waf-policies", "plan").run({ name: "strict", spec: "" })).resolves.toBeDefined();
  });

  it("will not plan a route without a gateway", async () => {
    await expect(action("ingress-routes", "plan").run({ name: "shop", spec: '{"hosts":["a.example.com"]}' })).rejects.toThrow(FormError);
  });

  it("sets a gateway secret file", async () => {
    await expect(action("ingress-gateways", "set-secret").run({ name: "edge", secret_name: "dns.env", content: "TOKEN=x" })).resolves.toEqual({ ok: true });
    await expect(action("ingress-gateways", "set-secret").run({ name: "edge", secret_name: "dns.env" })).rejects.toThrow(FormError);
  });
});
