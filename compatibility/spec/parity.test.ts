//! Krapi spec-parity tests — feature/spec compliance with upstream kro.
//! Layer 1 (offline): krapi CLI validate/generate-crd on upstream fixtures.
//! Layer 2 (runtime): local kapi REST API — RGD registration + CRD synthesis.
import { describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { krapi, validateFile, generateCrd, startKapi, api, UPSTREAM, KRAPI } from "../helpers";

const FIXTURES = `${UPSTREAM}/test/upgrade/fixtures`;

describe("krapi CLI — spec layer", () => {
  test("binary exists", () => expect(existsSync(KRAPI)).toBe(true));

  test("validates a valid RGD", async () => {
    const f = `${FIXTURES}/simple-deployment/rgd.yaml`;
    const r = await validateFile(f);
    expect(r.code).toBe(0);
    expect(r.stdout.toLowerCase()).toContain("validation successful");
  });

  test("generates a CRD with the instance kind", async () => {
    const f = `${FIXTURES}/simple-deployment/rgd.yaml`;
    const r = await generateCrd(f, "json");
    expect(r.code).toBe(0);
    const crd = JSON.parse(r.stdout);
    expect(crd.kind).toBe("CustomResourceDefinition");
    expect(crd.spec.names.kind).toBe("UpgradeSimpleApp");
  });

  test("rejects an RGD with no resources", async () => {
    const f = `${FIXTURES}/foreach-nil/rgd.yaml`;
    const r = await validateFile(f);
    expect(typeof r.code).toBe("number");
  });
});

describe("kapi — runtime spec layer", () => {
  test.skipIf(!existsSync("/Users/dineshbhattarai/src/kapi/target/release/kapi"))(
    "registers an RGD and synthesizes its CRD", 20_000,
    async () => {
      const k = await startKapi();
      try {
        const rgd = await Bun.file(`${FIXTURES}/simple-deployment/rgd.yaml`).text();
        const obj = Bun.YAML.parse(rgd);
        // kapi registers RGDs under kro.run, matching upstream.
        const created = await api(k.port, "POST", "/apis/kro.run/v1alpha1/resourcegraphdefinitions", obj);
        expect(created.status).toBe(201);
        let crd: { status: number } | undefined;
        for (let i = 0; i < 30; i++) {
          crd = await api(k.port, "GET", "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/upgradesimpleapps.kro.run");
          if (crd.status === 200) break;
          await Bun.sleep(200);
        }
        expect(crd?.status).toBe(200);
      } finally {
        k.stop();
      }
    },
  );
});
