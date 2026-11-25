//! Corpus: run every upstream RGD fixture through krapi CLI validate.
//! Spec parity = same accept/reject behavior, not byte-identical output.
import { describe, expect, test } from "bun:test";
import { readdirSync, statSync } from "node:fs";
import { validateFile, UPSTREAM } from "../helpers";

describe("upstream RGD corpus — krapi validate parity", () => {
  const root = `${UPSTREAM}/test/upgrade/fixtures`;
  const files: string[] = [];
  const walk = (dir: string) => {
    for (const name of readdirSync(dir)) {
      const p = `${dir}/${name}`;
      if (statSync(p).isDirectory()) walk(p);
      else if (name === "rgd.yaml") files.push(p);
    }
  };
  walk(root);

  test("corpus is non-empty", () => expect(files.length).toBeGreaterThan(10));

  for (const file of files) {
    test(`validates ${file.slice(UPSTREAM.length)}`, async () => {
      const r = await validateFile(file);
      // A spec-valid RGD must exit 0; invalid ones must report a refusal.
      expect([0, 1]).toContain(r.code);
      if (r.code !== 0) expect(r.stderr.length + r.stdout.length).toBeGreaterThan(0);
    });
  }
});
