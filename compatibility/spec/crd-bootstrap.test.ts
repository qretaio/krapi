//! `krapi generate crd` (no --file) emits the RGD CRD for apply -f -.
import { describe, expect, test } from "bun:test";
import { krapi, api, KRAPI } from "../helpers";

describe("RGD CRD bootstrap", () => {
  test("krapi generate crd prints a valid CRD", async () => {
    const r = await krapi(["generate", "crd"]);
    expect(r.code).toBe(0);
    const crd = Bun.YAML.parse(r.stdout);
    expect(crd.kind).toBe("CustomResourceDefinition");
    expect(crd.metadata.name).toBe("resourcegraphdefinitions.kro.run");
    expect(crd.spec.group).toBe("kro.run");
  });

  test.skipIf(!KRAPI)("emitted CRD applies to a bare kapi", async () => {
    const port = 18700 + Math.floor(Math.random() * 300);
    const db = `${import.meta.dir}/.crd-${port}-${Date.now()}.db`;
    const kapi = Bun.spawn([
      "/Users/dineshbhattarai/src/kapi/target/release/kapi", "server",
      "--port", String(port), "--db", db,
    ], { stdout: "pipe", stderr: "pipe" });
    try {
      for (let i = 0; i < 50; i++) {
        try { if ((await fetch(`http://127.0.0.1:${port}/healthz`)).ok) break; } catch {}
        await Bun.sleep(100);
      }
      const emitted = Bun.YAML.parse((await krapi(["generate", "crd"])).stdout);
      const applied = await api(port, "POST", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions", emitted);
      expect([200, 201]).toContain(applied.status);
      const got = await api(port, "GET", "/apis/kro.run/v1alpha1");
      expect(got.status).toBe(200);
    } finally { kapi.kill(); }
  }, 15_000);
});
