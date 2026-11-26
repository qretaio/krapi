//! End-to-end: kapi (CRD seed only) + krapi as a separate process.
import { describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { KRAPI, UPSTREAM, api } from "../helpers";

describe("decoupled deployment", () => {
  test.skipIf(!existsSync(KRAPI))("krapi reconciles an RGD over kapi HTTP", async () => {
    const port = 18600 + Math.floor(Math.random() * 500);
    const db = `${import.meta.dir}/.e2e-${port}-${Date.now()}.db`;
    const kapi = Bun.spawn([
      "/Users/dineshbhattarai/src/kapi/target/release/kapi", "server",
      "--port", String(port), "--db", db, "--with-controller",
    ], { stdout: "pipe", stderr: "pipe" });
    let krapi: ReturnType<typeof Bun.spawn> | undefined;
    try {
      for (let i = 0; i < 50; i++) {
        try { if ((await fetch(`http://127.0.0.1:${port}/healthz`)).ok) break; } catch {}
        await Bun.sleep(100);
      }
      krapi = Bun.spawn([KRAPI, "run", "--server", `http://127.0.0.1:${port}`,
        "--namespace", "default", "--rgd-group", "kro.run", "--instance-group", "kro.run"],
        { stdout: "pipe", stderr: "pipe" });
      await Bun.sleep(1500);

      const rgd = Bun.YAML.parse(await Bun.file(
        `${UPSTREAM}/test/upgrade/fixtures/simple-deployment/rgd.yaml`).text());
      const created = await api(port, "POST", "/apis/kro.run/v1alpha1/resourcegraphdefinitions", rgd);
      expect(created.status).toBe(201);

      let ready = false;
      for (let i = 0; i < 50; i++) {
        const s = await api(port, "GET", `/apis/kro.run/v1alpha1/resourcegraphdefinitions/${rgd.metadata.name}`);
        if (JSON.stringify(s.body?.status ?? {}).includes("Active")) { ready = true; break; }
        await Bun.sleep(200);
      }
      expect(ready).toBe(true);
    } finally {
      krapi?.kill(); kapi.kill();
    }
  }, 30_000);
});
