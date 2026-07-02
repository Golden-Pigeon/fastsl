#!/usr/bin/env python3
"""Capture golden API responses from a running swanboard for contract testing.

Usage:
    python tests/dump_golden.py http://localhost:5092 tests/golden

Writes one JSON file per endpoint into the output dir. These are the byte-for-byte
reference the Rust `fastsl` server must reproduce (see plan: Golden Reference strategy).
Timestamps / floats are captured as-is; the Rust contract test normalizes volatile
fields (SwanLab-Process-Time, absolute create/update times) before comparing.
"""
import json
import os
import sys
import urllib.parse
import urllib.request

def get(base, path):
    url = base.rstrip("/") + path
    with urllib.request.urlopen(url, timeout=300) as r:
        return json.loads(r.read().decode("utf-8"))

def save(outdir, name, obj):
    path = os.path.join(outdir, name + ".json")
    with open(path, "w", encoding="utf-8") as f:
        json.dump(obj, f, ensure_ascii=False, indent=2, sort_keys=True)
    print(f"wrote {path}")

def main():
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(1)
    base, outdir = sys.argv[1], sys.argv[2]
    os.makedirs(outdir, exist_ok=True)

    # Project-level endpoints.
    project = get(base, "/api/v1/project")
    save(outdir, "project", project)
    save(outdir, "project_charts", get(base, "/api/v1/project/charts"))
    # summaries can be very slow on the original server; capture but tolerate failure.
    try:
        save(outdir, "project_summaries", get(base, "/api/v1/project/summaries"))
    except Exception as e:  # noqa: BLE001
        print(f"WARN summaries failed on reference server: {e}")

    # Pick the first experiment and a couple of its tags for per-experiment fixtures.
    exps = project.get("data", {}).get("experiments", [])
    if not exps:
        print("no experiments found; skipping experiment fixtures")
        return
    eid = exps[0]["experiment_id"]
    save(outdir, f"experiment_{eid}", get(base, f"/api/v1/experiment/{eid}"))
    save(outdir, f"experiment_{eid}_summary", get(base, f"/api/v1/experiment/{eid}/summary"))
    save(outdir, f"experiment_{eid}_chart", get(base, f"/api/v1/experiment/{eid}/chart"))
    save(outdir, f"experiment_{eid}_status", get(base, f"/api/v1/experiment/{eid}/status"))
    try:
        save(outdir, f"experiment_{eid}_requirements",
             get(base, f"/api/v1/experiment/{eid}/requirements"))
    except Exception as e:  # noqa: BLE001
        print(f"WARN requirements failed: {e}")

    # One tag time-series (first non-system tag from the chart response).
    summary = get(base, f"/api/v1/experiment/{eid}/summary")
    tags = [s["key"] for s in summary.get("data", {}).get("summaries", []) if isinstance(s, dict)]
    if tags:
        tag = tags[0]
        enc = urllib.parse.quote(tag, safe="")
        save(outdir, f"experiment_{eid}_tag",
             get(base, f"/api/v1/experiment/{eid}/tag/{enc}"))

if __name__ == "__main__":
    main()
