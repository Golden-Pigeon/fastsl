# CLAUDE.md

Guidance for working in this repository. For deep implementation notes and per-task history,
see `PROGRESS.md` (source of truth); this file is the operational quick reference.

## What this is

**fastsl** — a fast, read-only viewer for local SwanLab logs. A Rust (axum) backend that is
**byte-compatible** with swanboard's `/api/v1` and embeds swanboard's Vue UI, so the dashboard
looks identical while loading orders of magnitude faster on large/slow logdirs (the original
`swanlab watch` / swanboard hangs under training IO; see `PROGRESS.md` for measured numbers).

One command serves both UI and API from a single process/port:

```bash
fastsl --logdir <swanlog dir> [--host 127.0.0.1] [--port 5092] [--cache-dir <fast dir>]
```

- `--logdir` (required): the folder containing `run-*` dirs and `runs.swanlab`.
- `--host`: bind address (default `127.0.0.1`; use `0.0.0.0` to expose on the LAN).
- `--port`: default `5092`.
- `--cache-dir`: tmpfs/SSD dir for the persistent summary sidecar (falls back to in-memory).

## Core invariant: read-only

`runs.swanlab` is written continuously by the live SwanLab SDK. **The backend opens it strictly
read-only and must never write it.** View preferences (pin/hide/collapse/show) are persisted to a
JSON sidecar `<logdir>/fastsl-ui.json`, and merged over DB values on read. Any change that could
write the training DB is a bug. (`-wal`/`-shm` are SQLite's coordination files, touched by any
reader — not a violation.)

## Byte-compat contract: do NOT "fix" replicated quirks

The API is a byte-for-byte replica of **swanboard 0.1.10b2**. Several swanboard behaviors look like
bugs but are replicated on purpose (different tag fields for summaries, `charts:null` on nested
projects, a shared `source_map`, ULP float noise, LTTB sampling). See "Non-obvious gotchas" in
`PROGRESS.md` before changing any endpoint output — validate against golden fixtures, don't
"correct" them.

## Repository layout

- `src/` — Rust backend. Entry `main.rs` (CLI, router, embedded assets, watcher); `api.rs`
  (axum handlers); `db.rs` (read-only rusqlite + all query logic); `resp.rs` (response envelope);
  `uistate.rs` (read-only view-preference overlay + sidecar); `summary_cache.rs` (summary cache).
- `build.rs` — builds the frontend from the submodule before compiling (see below).
- `frontend/` — **git submodule** → fork of `SwanHubX/SwanLab-Dashboard` (branch `fastsl`).
  Vue3+Vite source in `frontend/vue/`; Vite output `frontend/swanboard/template/` is what gets embedded.
- `tests/dump_golden.py` — captures golden API JSON from a running swanboard for contract diffs.

## Build

Requires a **Rust toolchain**, **Node.js + npm** (for the frontend build), and the submodule.

```bash
git submodule update --init --recursive   # first checkout (build.rs errors without it)
cargo build                                # debug
cargo build --release                      # optimized, self-contained single binary
```

`build.rs` runs `npm install` (only if `node_modules` is absent) then `npm run build.release` in
`frontend/`, producing the bundle that `rust-embed` embeds. It only re-runs when frontend files
change (`rerun-if-changed` on `frontend/vue`, `package.json`, `vite.config.js`), so ordinary Rust
edits do **not** trigger an npm build.

- Node-less / offline rebuild (reuse an already-built bundle): `FASTSL_SKIP_FRONTEND_BUILD=1 cargo build`.

## Test

```bash
cargo test                    # unit tests (summary cache, etc.)
# Contract check against the live original (swanboard on :5092):
python tests/dump_golden.py http://localhost:5092 tests/golden
# then run fastsl on another port and diff endpoint outputs.
```

## Running the server (for local testing)

- **Do not leave the server running** — start it only for a test, then `pkill -x fastsl`. The user
  manages long-running services.
- Start via the harness **background bash** (`run_in_background: true`); a plain `&` gets reaped
  when the launching call's process group is cleaned up.
- `curl` must bypass the proxy: `curl --noproxy '*' http://127.0.0.1:PORT/...` (else 502).

## Frontend workflow (submodule)

The UI is customized in the fork and synced from upstream; see the "Frontend workflow" section of
`PROGRESS.md` for the full recipe. In short:

- **Customize:** edit `frontend/vue/src/**` → commit + push in `frontend/` to the fork's `fastsl`
  branch → in the main repo `git add frontend` to move the submodule pointer. `cargo build` rebuilds.
- **Sync upstream:** in `frontend/`, merge `upstream/main` (remote =
  `https://github.com/SwanHubX/SwanLab-Dashboard`), resolve conflicts in `vue/`, push, then bump the
  pointer here.
- **API coupling (important):** the UI and the `/api/v1` contract are version-locked. After syncing a
  newer frontend, re-run the golden contract check — if the upstream API shape changed, the Rust
  handlers must be updated to match. Bump deliberately, not automatically.

## Conventions

- Git: conventional-commit prefixes (`feat:`, `fix:`, `refactor:`, …). No AI attribution trailer
  (disabled globally by the user).
- Keep the backend pure-Rust and self-contained; don't add runtime services or leave processes running.
- Cost/spend is not a concern for this project — don't surface budget warnings.
