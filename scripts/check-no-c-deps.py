#!/usr/bin/env python3
"""Pure-Rust gate: fail on any crate that links a native library (`-sys`) or has a build script that compiles C/C++.

Run locally exactly as CI does:  python3 scripts/check-no-c-deps.py
Exceptions: one crate name per line in scripts/c-deps-exceptions.txt (initially empty).
"""
import json, os, subprocess, sys

C_BUILD_DEPS = {"cc", "cmake", "bindgen", "cxx-build", "pkg-config", "vcpkg", "autocfg-c"}
here = os.path.dirname(os.path.abspath(__file__))
exc_file = os.path.join(here, "c-deps-exceptions.txt")
exceptions = set()
if os.path.exists(exc_file):
    exceptions = {l.strip() for l in open(exc_file) if l.strip() and not l.startswith("#")}

meta = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--format-version", "1", "--locked"] if os.path.exists("Cargo.lock")
    else ["cargo", "metadata", "--format-version", "1"]))
workspace = set(meta["workspace_members"])
checked, bad, used = 0, [], []
for p in meta["packages"]:
    if p["id"] in workspace:
        continue
    checked += 1
    reasons = []
    # A `links` key means the crate binds a native (C) library. Pure-Rust
    # `-sys` crates such as windows-sys / linux-raw-sys have neither `links`
    # nor a C build script and are allowed.
    if p.get("links"):
        reasons.append(f"links native library `{p['links']}`")
    if p["name"].endswith("-sys") and not reasons:
        print(f"note: {p['name']} is a -sys crate but has no native link or C build script (pure Rust)")
    build_deps = {d["name"] for d in p["dependencies"] if d["kind"] == "build"}
    has_build_rs = any("custom-build" in t["kind"] for t in p["targets"])
    if has_build_rs and build_deps & C_BUILD_DEPS:
        reasons.append("build.rs uses " + ", ".join(sorted(build_deps & C_BUILD_DEPS)))
    if reasons:
        if p["name"] in exceptions:
            used.append(p["name"])
            print(f"EXCEPTION: {p['name']} {p['version']} ({'; '.join(reasons)})")
        else:
            bad.append(f"{p['name']} {p['version']}: {'; '.join(reasons)}")
print(f"checked {checked} dependencies")
if bad:
    for b in bad:
        print("FORBIDDEN C/C++ dependency:", b)
    sys.exit(1)
print("pure-Rust gate passed")
