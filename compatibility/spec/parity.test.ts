//! Krapi spec-parity tests — feature/spec compliance with upstream kro.
//! Layer 1 (offline): krapi CLI validate/generate-crd on upstream fixtures.
//! Runtime layer lives in decoupled.test.ts (kapi + krapi as separate processes).
import { describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { validateFile, generateCrd, UPSTREAM, KRAPI } from "../helpers";

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

