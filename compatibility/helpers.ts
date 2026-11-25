//! Shared helpers: run the krapi CLI and a local kapi server.
import { $ } from "bun";

export const ROOT = new URL("..", import.meta.url).pathname;
export const KRAPI = process.env.KRAPI_BIN ?? `${ROOT}target/release/krapi`;
export const KAPI = process.env.KAPI_BIN ?? "/Users/dineshbhattarai/src/kapi/target/release/kapi";
export const UPSTREAM = `${ROOT}compatibility/upstream`;

/** Run krapi CLI, return {code, stdout, stderr}. */
export async function krapi(args: string[]) {
  const p = Bun.spawn([KRAPI, ...args], { stdout: "pipe", stderr: "pipe" });
  const [out, err, code] = await Promise.all([
    new Response(p.stdout).text(), new Response(p.stderr).text(), p.exited,
  ]);
  return { code, stdout: out, stderr: err };
}

/** Validate an RGD file offline — spec tier. */
export async function validateFile(path: string) {
  return krapi(["validate", "--file", path]);
}

/** Generate the instance CRD from an RGD — spec tier. */
export async function generateCrd(path: string, format: "yaml" | "json" = "yaml") {
  return krapi(["generate", "crd", "--file", path, "--format", format]);
}

/** Start a local kapi on a free port; returns {port, stop}. */
export async function startKapi() {
  const port = 18443 + Math.floor(Math.random() * 1000);
  const db = `${import.meta.dir}/.kapi-test-${port}-${Date.now()}.db`;
  const proc = Bun.spawn([KAPI, "server", "--port", String(port), "--db", db, "--with-controller"], {
    stdout: "pipe", stderr: "pipe",
  });
  // wait for readiness
  for (let i = 0; i < 50; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${port}/healthz`);
      if (r.ok) return { port, stop: () => proc.kill() };
    } catch {}
    await Bun.sleep(100);
  }
  proc.kill();
  throw new Error("kapi did not become ready");
}

/** POST JSON to kapi. */
export async function api(port: number, method: string, path: string, body?: unknown) {
  const r = await fetch(`http://127.0.0.1:${port}${path}`, {
    method,
    headers: { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await r.text();
  let json: unknown;
  try { json = JSON.parse(text); } catch { json = text; }
  return { status: r.status, body: json };
}
