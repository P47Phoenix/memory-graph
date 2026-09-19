#!/usr/bin/env python3
"""Tests for check-no-c-deps.py using a fixture workspace: python3 scripts/test_gate.py"""
import os, subprocess, sys, tempfile, textwrap

GATE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "check-no-c-deps.py")

def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    open(path, "w").write(textwrap.dedent(text))

def run(cwd, env=None):
    e = dict(os.environ, **(env or {}))
    return subprocess.run([sys.executable, GATE], cwd=cwd, env=e, capture_output=True, text=True)

with tempfile.TemporaryDirectory() as d:
    write(f"{d}/native-sys/Cargo.toml", '[package]\nname="native-sys"\nversion="0.1.0"\nedition="2021"\nlinks="native"\nbuild="build.rs"\n')
    write(f"{d}/native-sys/build.rs", "fn main(){}\n")
    write(f"{d}/native-sys/src/lib.rs", "")
    write(f"{d}/pure-sys/Cargo.toml", '[package]\nname="pure-sys"\nversion="0.1.0"\nedition="2021"\n')
    write(f"{d}/pure-sys/src/lib.rs", "")
    # Clean: only a pure-Rust -sys crate.
    write(f"{d}/clean/Cargo.toml", '[package]\nname="clean"\nversion="0.1.0"\nedition="2021"\n[dependencies]\npure-sys={path="../pure-sys"}\n')
    write(f"{d}/clean/src/lib.rs", "")
    r = run(f"{d}/clean")
    assert r.returncode == 0 and "checked 1 dependencies" in r.stdout, r.stdout + r.stderr
    # Dirty: native-linking crate must fail and be named.
    write(f"{d}/dirty/Cargo.toml", '[package]\nname="dirty"\nversion="0.1.0"\nedition="2021"\n[dependencies]\nnative-sys={path="../native-sys"}\n')
    write(f"{d}/dirty/src/lib.rs", "")
    r = run(f"{d}/dirty")
    assert r.returncode == 1 and "native-sys" in r.stdout, r.stdout + r.stderr
    # Exception passes for that crate only, and is printed.
    write(f"{d}/exc.txt", "native-sys\n")
    r = run(f"{d}/dirty", {"C_DEPS_EXCEPTIONS": f"{d}/exc.txt"})
    assert r.returncode == 0 and "EXCEPTION: native-sys" in r.stdout, r.stdout + r.stderr
    write(f"{d}/exc.txt", "other-crate\n")
    assert run(f"{d}/dirty", {"C_DEPS_EXCEPTIONS": f"{d}/exc.txt"}).returncode == 1
print("gate tests passed")
