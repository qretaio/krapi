//! Assert the compatibility corpus is pinned to the documented upstream tag.
import { describe, expect, test } from "bun:test";
import { $ } from "bun";
import { UPSTREAM } from "../helpers";

describe("upstream pin", () => {
  test("corpus is at the documented tag", async () => {
    const p = await $`git -C ${UPSTREAM} describe --tags --exact-match`.quiet().nothrow();
    expect(p.exitCode).toBe(0);
    expect(p.stdout.toString().trim()).toBe("v0.10.0");
  });

  test("API group is kro.run/v1alpha1", async () => {
    const f = Bun.file(`${UPSTREAM}/api/v1alpha1/groupversion_info.go`);
    const src = await f.text();
    expect(src).toContain('KRODomainName = "kro.run"');
    expect(src).toContain('Version: "v1alpha1"');
  });
});
