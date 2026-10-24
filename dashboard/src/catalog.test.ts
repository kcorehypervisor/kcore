import { create, type DescMessage, type DescService, type Message } from "@bufbuild/protobuf";
import { createRouterTransport, type ConnectRouter } from "@connectrpc/connect";
import { beforeAll, describe, expect, it } from "vitest";
import { RESOURCES } from "./catalog";
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

function serveUnary(router: ConnectRouter, service: DescService) {
  const impl: Record<string, () => Message> = {};
  for (const method of service.methods) {
    if (method.methodKind === "unary") impl[method.localName] = () => sample(method.output);
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
    if (!resource.inspect) continue;
    it(`inspects ${resource.id}`, async () => {
      const rows = (await resource.list?.({})) ?? [];
      const key = String(rows[0]?.[resource.idField] ?? "sample");
      const detail = await resource.inspect!(key);
      expect(detail.detail).toBeDefined();
    });
  }
});
