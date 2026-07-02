# fastsl — implementation progress & handoff notes

Fast, read-only viewer for SwanLab local logs. Rust (axum) backend that is **byte-compatible**
with swanboard's `/api/v1` and serves the **original swanboard Vue bundle** (embedded), so the UI
is identical. Single command: `fastsl --logdir <swanlog dir> [--port 5092] [--host] [--cache-dir]`.

Design plan: `/data/python/.claude/plans/starry-wibbling-crab.md` (方案 B).

## Status (tasks)

| Task | Scope | State |
|---|---|---|
| T1 | CLI + embedded Vue static + SPA fallback + version headers | ✅ done, verified |
| T2 | `/project`, `/experiment/{id}` (rusqlite read-only) | ✅ byte-identical vs :5092 |
| T3 | `/project/summaries`, `/experiment/{id}/summary` | ✅ equal (mod float/live) |
| T4 | `/project/charts`, `/experiment/{id}/chart`, `/status` | ✅ byte-identical |
| T5 | `/experiment/{id}/tag/{tag}` + LTTB | ✅ byte-identical, LTTB point-for-point |
| T6-read | `recent_log`, `requirements`, `media` | ✅ byte-identical; media served via `ServeFile` (Range/206 + conditional headers, matches FastAPI `FileResponse`) |
| T6-write | PATCH `namespace/{id}/opened`, `chart/{id}/status`, `experiment/{id}/show` | ✅ done, verified (sidecar overlay; runs.swanlab mtime unchanged) |
| T7 | release build, startup pre-warm of summaries, `notify` incremental refresh, `--cache-dir` persistence | ✅ done, verified |

All GET endpoints the Vue dashboard needs for **viewing** are implemented and verified against the
live original on `http://localhost:5092`. Reference logdir: `/data/python/depth_completion/swanlog`
(395 experiments, ~37k files, 130 GB).

## Perf (fastsl vs original under training IO load)
- `/project`: 0.56s vs **548s**. `/project/charts`: 0.47s vs 62.5s. `/experiment/{id}/summary`: 1.7ms vs 8.4s.
- `/project/summaries`: warm **41ms** vs original **hangs** (>120s; forced once at 215s). Cold fastsl ~258s
  (15k tiny reads under IO contention) → T7 startup pre-warm + `--cache-dir` fixes this.

## Source layout (`src/`)
- `main.rs` — clap args, axum router, embedded assets (`rust_embed` of `frontend_dist/`), SPA fallback, `SwanLab-Version`/`SwanLab-Process-Time` headers, `AppState { logdir, summary_cache, ui }`, startup summary prewarm, `notify` watcher + 30s polling fallback.
- `summary_cache.rs` — **T7 summary cache**: moka in-memory cache plus optional persistent JSON sidecar `<cache-dir>/fastsl-summary-cache.json`, keyed by `run_id/folder/last_shard_mtime_ns/last_shard_size` and isolated by canonical logdir.
- `resp.rs` — envelope `{code,message,data}` (0/3404/3409/3500), matches `module/resp.py`.
- `db.rs` — all read logic: rusqlite RO; `project_info`, `experiment_info`, `project_summaries`,
  `experiment_summary` (moka cache + tail-read), charts (`project_charts`/`build_experiment_charts`/
  `experiment_status`), `tag_data` + `lttb`, `recent_logs`/`requirements`/`media_file`.
  Read fns take a `&UiOverlay` and merge chart.status / namespace.opened / experiment.show /
  project|exp pinned_opened|hidden_opened on top of the DB values (empty overlay = no-op,
  byte-identical to T2–T5). Also: `namespace_exists`/`experiment_exists`/`project_exists`/
  `chart_owner`/`chart_status_groups`/`experiment_dict` for PATCH support.
- `uistate.rs` — **T6-write overlay**: `UiOverlay` (7 int→int override maps) + `UiState`
  (thread-safe, atomic JSON sidecar `<logdir>/fastsl-ui.json`). `truthy_to_int` mirrors
  swanboard's `1 if value else 0`. `runs.swanlab` stays strictly read-only.
- `api.rs` — axum handlers (blocking work via `spawn_blocking`); GETs pass `ui.snapshot()`,
  PATCHs (`patch_namespace_opened`/`patch_chart_status`/`patch_experiment_show`) write only the sidecar.
  `get_media` resolves the path then delegates to `tower_http::services::ServeFile` (Range/206,
  `Accept-Ranges`, `Last-Modified`/conditional GET) so it matches swanboard's FastAPI `FileResponse`.
- `frontend_dist/` — copied verbatim from swanboard `template/` (index.html + 34 assets); embedded at build.
- `tests/dump_golden.py` — capture golden API JSON from a running swanboard for contract diffs.

## Non-obvious gotchas (replicated swanboard quirks — DO NOT "fix")
1. **summaries use different tag folder fields**: `/project/summaries` reads by `tag.sort`;
   `/experiment/{id}/summary` reads by `tag.folder`. When sort≠folder they diverge — this is
   swanboard's real (buggy) behavior; replicate exactly.
2. **nested project `charts` = null**: `Project.__dict__()` (top-level, and proj-charts chart `project_id`)
   gives the real int; the project reached via `model_to_dict` FK recursion (namespaces' nested project)
   gives `charts: null`. See `nullify_charts`.
3. **shared `source_map` in `/project/charts`**: swanboard builds ONE `source_map` dict across all charts,
   so every chart carries the accumulated union of all contributing experiments (303), while `source`
   stays per-chart (178). Replicated by assigning the shared map post-loop.
4. **LTTB**: ported verbatim (`lttb`/`sample_a_bucket`/`calc_bucket_capacity`); shard `.log` files chosen
   by **lexical** sort (matches `get_tag_files` + `.sort()`); last shard's last line = latest value.
5. **float noise**: original parses with `ujson` (imprecise); fastsl uses serde_json (correct). Expect
   last-ULP diffs on some values — benign, invisible in UI. Contract diffs must tolerate rel ~1e-12.
6. Tag names contain `/` → route is `/experiment/:id/tag/*tag`.

## Testing workflow (IMPORTANT operational notes)
- **Do NOT leave the server running** — start only for a test, then `pkill -x fastsl`. (User manages services.)
- Starting the server for tests: use the harness **background bash** (`run_in_background: true`) —
  a plain `&` inside a Bash call gets killed when that call's process group is cleaned up. Run
  `target/debug/fastsl --logdir ... --port 5099` as a background task; it persists across calls.
- `curl` must bypass the proxy: `curl --noproxy '*' http://127.0.0.1:PORT/...` (proxy returns 502 otherwise).
- Contract check: `python tests/dump_golden.py http://localhost:5092 tests/golden` then diff vs `:5099`.
- Incremental builds ~4–10s normally; can hit minutes under heavy training IO. Deps are cached (first
  build ~9min was one-time). If a build wedges (cargo alive, 0 rustc), kill it — but never `pkill -f 'cargo build'`
  (it matches your own shell); target by pid.

## Next up
All planned milestones (T1–T7) are implemented and locally verified. Dead code removed:
the obsolete `paths.rs` module (superseded by db.rs's traversal-hardened `safe_run_path`) is
deleted, and both `cargo build` and `cargo build --release` are now warning-free. Remaining
optional work: broader contract fixture automation and packaging/install docs.

## T7 verification (done)
- Added `--cache-dir` persistence through `<cache-dir>/fastsl-summary-cache.json`; persisted entries are keyed by
  `run_id/folder/last_shard_mtime_ns/last_shard_size`, so appends produce a new key and stale values are ignored.
- Added startup background prewarm via `db::prewarm_project_summaries`, which reuses the verified `/project/summaries`
  logic and flushes the persistent sidecar.
- Added recursive `notify` watcher for `.log`/`.json` changes plus a 30s polling fallback; watcher events are throttled
  and call the same verified prewarm path.
- Unit tests: `cargo test summary_cache -- --nocapture` covers persistence reload and logdir isolation.
- Full verification: `cargo test && cargo build && cargo build --release` passed (only warning: unused `resp::conflict`).
- Runtime check on a minimal temp swanlog: startup prewarm created cache file and served `loss=1.25`; appending a log
  line triggered `summary cache notify prewarm` and `/project/summaries` updated to `loss=1.5`; `runs.swanlab` mtime unchanged; server stopped and port 5099 free.

## T6-write verification (done)
Copied `runs.swanlab` to a temp dir (PATCH paths only need the DB) and exercised every endpoint:
- `PATCH /chart/{id}/status` {1,0,-1}: pin→pinned(-1) ns, hide→hidden(-2) ns, restore→back; response
  `{groups:[...]}` with full chart objects inlined. Works for both project charts (id=2/4) and
  experiment charts (id=1, routed via `chart_owner` → `experiment_charts`).
- `PATCH /namespace/{id}/opened`: real ns → collapse (opened=0); dynamic -1/-2 → toggles
  project|experiment `pinned_opened`/`hidden_opened`. Response `{code:0,message:"success"}` (no data).
- `PATCH /experiment/{id}/show` {bool}: returns `{experiment:<Experiment.__dict__()>}` with nested
  project_id; merged `show` reflects on `GET /project`.
- **Read-only invariant:** `runs.swanlab` main-file mtime unchanged across all PATCHs (the `-wal`/`-shm`
  are SQLite's standard WAL coord files, touched by any reader incl. original swanboard — not db content).
- **Persistence:** overrides reload from `fastsl-ui.json` after a full restart.
- **Errors match swanboard:** missing param→422; missing exp/proj id on dynamic ns→422;
  nonexistent chart→`3422 "chart_id not exist"`; nonexistent ns/exp→404.
