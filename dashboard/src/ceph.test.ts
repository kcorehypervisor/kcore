import { describe, expect, it } from "vitest";
import { cephCapacity, cephHealth, cephMembers, cephPools } from "./ceph";

const RAW_WARN = JSON.stringify({
  health: {
    status: "HEALTH_WARN",
    checks: {
      OSDMAP_FLAGS: { severity: "HEALTH_WARN", summary: { message: "noout,norebalance flag(s) set" } },
      PG_DEGRADED: { severity: "HEALTH_WARN", summary: { message: "Degraded data redundancy: 12 pgs degraded" } },
    },
  },
  quorum_names: ["node-103", "node-113"],
  monmap: { num_mons: 3 },
  osdmap: { num_osds: 3, num_up_osds: 2, num_in_osds: 3 },
  pgmap: {
    num_pgs: 33,
    pgs_by_state: [
      { state_name: "active+clean", count: 21 },
      { state_name: "active+undersized+degraded+remapped+backfill_wait", count: 12 },
    ],
    bytes_total: 3000,
    bytes_used: 1000,
    bytes_avail: 2000,
  },
});

describe("cephHealth", () => {
  it("prefers the typed health", () => {
    const health = cephHealth({
      ceph_status_json: RAW_WARN,
      typed_health: { health_status: "HEALTH_OK", mon_quorum: ["a", "b", "c"], mon_count: 3, osd_total: 3, osd_up: 3, osd_in: 3, pgs_total: 33, noout: true, flags: ["noout"], checks: [] },
    });
    expect(health.source).toBe("typed");
    expect(health.status).toBe("HEALTH_OK");
    expect(health.flags).toEqual(["noout"]);
    expect(health.raw).toMatchObject({ health: { status: "HEALTH_WARN" } });
  });

  it("parses ceph -s when the controller sent no typed health", () => {
    const health = cephHealth({ ceph_status_json: RAW_WARN, typed_health: null });
    expect(health.source).toBe("raw");
    expect(health.quorum).toEqual(["node-103", "node-113"]);
    expect(health.monCount).toBe(3);
    expect([health.osdTotal, health.osdUp, health.osdIn]).toEqual([3, 2, 3]);
    expect(health.pgsNotClean).toBe(12);
    expect(health.backfill).toBe(12);
    expect(health.flags).toEqual(["noout", "norebalance"]);
    expect(health.checks.map((check) => check.type)).toEqual(["OSDMAP_FLAGS", "PG_DEGRADED"]);
    expect(health.bytes).toEqual({ total: 3000, used: 1000, avail: 2000 });
  });

  it("survives an empty or broken status", () => {
    expect(cephHealth({}).source).toBe("none");
    expect(cephHealth({ ceph_status_json: "{not json" }).source).toBe("none");
  });
});

describe("cephCapacity", () => {
  it("uses ceph df and keeps device classes with space", () => {
    const capacity = cephCapacity(
      {
        capacity: {
          ok: true,
          queried_node: "node-103",
          total_bytes: 100,
          used_bytes: 40,
          avail_bytes: 60,
          by_class: [
            { device_class: "ssd", total_bytes: 100, used_bytes: 40, avail_bytes: 60 },
            { device_class: "hdd", total_bytes: 0 },
          ],
        },
      },
      cephHealth({}),
    );
    expect(capacity?.source).toBe("ceph df on node-103");
    expect(capacity?.classes.map((item) => item.name)).toEqual(["ssd"]);
  });

  it("falls back to ceph -s bytes and keeps the error", () => {
    const capacity = cephCapacity({ capacity_error: "no member answered" }, cephHealth({ ceph_status_json: RAW_WARN }));
    expect(capacity).toMatchObject({ total: 3000, source: "ceph -s", note: "no member answered" });
  });
});

describe("cephPools", () => {
  it("shows the default kcore-vms pool when the spec names none", () => {
    const pools = cephPools({ size: 3, min_size: 2, pools: [] }, { pools: [{ name: "kcore-vms", stored_bytes: 10, percent_used_bp: 1234 }, { name: ".mgr" }] });
    expect(pools).toHaveLength(2);
    expect(pools[0]).toMatchObject({ name: "kcore-vms", managed: "default", replicas: "3/2", pg_num: "autoscale", used: "12.3%" });
    expect(pools[1]).toMatchObject({ name: ".mgr", managed: "not in spec" });
  });

  it("lists declared pools with their own replica counts", () => {
    const pools = cephPools({ size: 3, min_size: 2, pools: [{ name: "fast", application: "rbd", size: 2, min_size: 1, pg_num: 64 }] }, null);
    expect(pools).toEqual([expect.objectContaining({ name: "fast", managed: "spec", replicas: "2/1", pg_num: 64, stored_bytes: null })]);
  });
});

describe("cephMembers", () => {
  it("marks lost and draining members", () => {
    const members = cephMembers(
      { nodes: [{ node_id: "node-103", osd_device: "/dev/sda", osd_devices: ["/dev/sda", "/dev/sdb"] }, { node_id: "node-1" }], lost_members: ["node-118"] },
      { operation_message: "draining node-103: 12 PGs backfilling" },
    );
    expect(members.map((member) => [member.node_id, member.state])).toEqual([
      ["node-103", "draining"],
      ["node-1", null],
      ["node-118", "lost"],
    ]);
    expect(members[0].osd_devices).toEqual(["/dev/sda", "/dev/sdb"]);
    expect(members[0].role).toBe("full");
  });
});
