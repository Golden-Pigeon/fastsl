# fastsl

[![crates.io](https://img.shields.io/crates/v/fastsl.svg)](https://crates.io/crates/fastsl)
[![CI](https://github.com/Golden-Pigeon/fastsl/actions/workflows/ci.yml/badge.svg)](https://github.com/Golden-Pigeon/fastsl/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A fast, **read-only** viewer for local [SwanLab](https://github.com/SwanHubX/SwanLab) logs.

`fastsl` is a Rust ([axum](https://github.com/tokio-rs/axum)) backend that is **byte-compatible**
with swanboard's `/api/v1` and embeds swanboard's own Vue dashboard — so the UI is identical to
`swanlab watch`, but it loads orders of magnitude faster on large or busy log directories.

It exists because the stock `swanlab watch` / swanboard becomes unusable on big logdirs under
training IO — e.g. on a ~400-experiment / ~37k-file / 130 GB directory, `/api/v1/project` took
**548 s** and `/api/v1/project/summaries` **hung past 120 s**. fastsl serves the same data in
**sub-second** time from a single command.

| Endpoint | swanboard | fastsl |
|---|---|---|
| `/project` | 548 s | 0.56 s |
| `/project/charts` | 62.5 s | 0.47 s |
| `/project/summaries` | hangs (>120 s) | 41 ms (warm) |
| `/experiment/{id}/summary` | 8.4 s | 1.7 ms |

## Features

- **Drop-in UI** — embeds the original swanboard Vue bundle; the dashboard looks and behaves the same.
- **Single command, single binary** — serves UI and API from one process/port.
- **Read-only & safe** — opens `runs.swanlab` strictly read-only; the live SwanLab SDK writer is
  never disturbed. View preferences (pin/hide/collapse/show) go to a JSON sidecar, not the training DB.
- **Fast** — read-only SQLite with JOINs replacing swanboard's N+1 queries, a summary cache
  (in-memory + optional persistent sidecar) with startup prewarm and incremental refresh, and a
  ported LTTB downsampler for time series.
- **Range-capable media** — images/audio/video served with HTTP Range support.

## Install

### Prebuilt binaries (no toolchain needed)

Download the archive for your platform from the
[latest release](https://github.com/Golden-Pigeon/fastsl/releases/latest), extract it, and run the
`fastsl` binary. Each archive is a single self-contained executable (the UI is embedded).

| OS | x86_64 | arm64 |
|---|---|---|
| Linux (static, glibc-independent) | `x86_64-unknown-linux-musl` | `aarch64-unknown-linux-musl` |
| macOS | `x86_64-apple-darwin` | `aarch64-apple-darwin` |
| Windows | `x86_64-pc-windows-gnu` | `aarch64-pc-windows-msvc` |

The Linux builds are statically linked (built with [Zig](https://ziglang.org/) as the linker via
`cargo-zigbuild`), so they run on any Linux distro regardless of its glibc version.

### From crates.io

```bash
cargo install fastsl
```

The published crate ships the prebuilt UI bundle, so this needs only a Rust toolchain — no Node.js.

## Usage

```bash
fastsl --logdir <swanlog dir> [--host 127.0.0.1] [--port 5092] [--cache-dir <fast dir>]
```

- `--logdir` (required) — the directory containing your `run-*` folders and `runs.swanlab`.
- `--host` — bind address; default `127.0.0.1`, use `0.0.0.0` to expose on your network.
- `--port` — default `5092`.
- `--cache-dir` — a fast (tmpfs/SSD) directory for the persistent summary cache; optional.

Then open `http://<host>:<port>/` in a browser.

## Build from source

Building from source rebuilds the Vue frontend, so it additionally needs **Node.js + npm** and the
frontend git submodule:

```bash
git clone --recurse-submodules https://github.com/Golden-Pigeon/fastsl
cd fastsl
cargo build --release        # produces a self-contained ./target/release/fastsl
```

`build.rs` builds the frontend (`npm run build.release` in the `frontend/` submodule) and embeds the
result into the binary; it only re-runs when frontend sources change. To skip the Node build and
reuse an already-built bundle, set `FASTSL_SKIP_FRONTEND_BUILD=1`.

## How it works

fastsl replicates the swanboard `/api/v1` contract exactly (targeting swanboard **0.1.10b2**) and
serves the same embedded Vue UI, so no frontend rewrite is needed. The backend gains the speedups
via read-only SQLite, JOIN-based queries, caching, and tail-reads instead of full-file scans. View
interactions are layered on top of the DB via a read-only overlay persisted to
`<logdir>/fastsl-ui.json`.

The frontend lives in the `frontend/` git submodule (a fork of `SwanHubX/SwanLab-Dashboard`), which
can be customized locally and re-synced from upstream. See `PROGRESS.md` and `CLAUDE.md` for the
development and frontend-sync workflows.

## Project status

Core functionality (viewing, charts, summaries, media, time series, view-preference persistence) is
implemented and verified byte-for-byte against a live swanboard. This is a read-only viewer: it does
not support run mutation (stop/delete/rename).

## Acknowledgements

The dashboard UI and API contract come from [SwanLab](https://github.com/SwanHubX/SwanLab) /
[SwanLab-Dashboard](https://github.com/SwanHubX/SwanLab-Dashboard) by the SwanHub team. fastsl is an
independent, compatible backend that reuses that UI. It is licensed under Apache-2.0 (see `LICENSE`
and `NOTICE`).
