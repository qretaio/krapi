# Upstream kro compatibility target

krapi targets **only the latest upstream kro release** — no backward
compatibility with older kro versions.

| Field | Value |
|---|---|
| Upstream repo | https://github.com/kro-run/kro |
| Pinned tag | `v0.10.0` |
| Pinned commit | `3bc58f0e76d9ed57c286696f178058752458134e` (2026-11-06) |
| API group/version | `kro.run/v1alpha1` |
| Ownership label prefix | `kro.run/` |
| Local checkout | `compatibility/upstream/` (gitignored, clone + checkout the tag) |

## Refresh procedure

1. `cd compatibility/upstream && git fetch --tags && git checkout <new-latest-tag>`
2. Update this file (tag, commit, date).
3. Run `cd compatibility && bun test spec/` — all parity tests must pass.
4. If a behavior changed upstream, port it into krapi (do not add a
   compatibility shim for the old behavior).

## Scope of parity

- **Feature/spec parity**, not byte-for-byte output parity.
- Offline layer: `krapi validate` / `generate crd` over upstream RGD fixtures.
- Runtime layer: local kapi REST API — RGD registration, CRD synthesis, status.
