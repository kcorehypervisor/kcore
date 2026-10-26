import assert from "node:assert/strict";
import test from "node:test";
import { findKind } from "../src/catalog.js";
import { buildApply, buildDelete, buildPlanCall, buildRead } from "../src/requests.js";

const route = {
  name: "shop",
  gateway: "edge",
  hosts: ["shop.example.com"],
  paths: [{ prefix: "/", backends: [{ kind: "vm", name: "web", port: 8080 }] }],
};

test("ingress kinds are in the catalog", () => {
  assert.equal(findKind("ingress-gateway")?.yamlKind, "IngressGateway");
  assert.equal(findKind("IngressRoute")?.id, "ingress-route");
  assert.equal(findKind("waf_policy")?.id, "waf-policy");
});

test("apply sends the name and the spec as JSON, without name or kind in the spec", () => {
  const kind = findKind("ingress-route")!;
  const [call] = buildApply(kind, { ...route, kind: "http" });
  assert.equal(call.method, "createIngressRoute");
  const request = call.request as { name: string; specJson: string };
  assert.equal(request.name, "shop");
  const spec = JSON.parse(request.specJson);
  assert.equal(spec.gateway, "edge");
  assert.equal(spec.name, undefined);
  assert.equal(spec.kind, undefined);
  assert.deepEqual(spec.hosts, ["shop.example.com"]);
});

test("read and delete address the right RPCs", () => {
  assert.equal(buildRead(findKind("ingress-gateway")!).method, "listIngressGateways");
  assert.equal(buildRead(findKind("ingress-gateway")!, "edge").method, "getIngressGateway");
  assert.equal(buildRead(findKind("waf-policy")!, "crs").method, "getWafPolicy");
  assert.equal(buildDelete(findKind("ingress-route")!, "shop").method, "deleteIngressRoute");
  assert.equal(buildDelete(findKind("waf-policy")!, "crs").method, "deleteWafPolicy");
});

test("plan renders the gateway the route belongs to", () => {
  const call = buildPlanCall(findKind("ingress-route")!, route);
  assert.equal(call.method, "planIngress");
  const request = call.request as { gateway: string; documents: Array<{ kind: string; name: string }> };
  assert.equal(request.gateway, "edge");
  assert.equal(request.documents[0].kind, "IngressRoute");
  const gw = buildPlanCall(findKind("ingress-gateway")!, { name: "edge", nodes: ["n1"] });
  assert.equal((gw.request as { gateway: string }).gateway, "edge");
  assert.throws(() => buildPlanCall(findKind("ingress-route")!, { name: "x", hosts: ["a.example.com"] }), /gateway/);
});

test("names are validated before they reach the controller", () => {
  assert.throws(() => buildApply(findKind("waf-policy")!, { name: "bad name" }));
  assert.throws(() => buildApply(findKind("waf-policy")!, {}));
});
