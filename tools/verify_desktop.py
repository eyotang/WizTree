"""Run native GPUI interaction checks against isolated, disposable fixtures.

Build with --features interaction-smoke. These checks dispatch GPUI events and
production callbacks; they do not automate OS mouse input or confirm deletion.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def verify(executable, output):
    with tempfile.TemporaryDirectory(prefix="wiztree-desktop-check-") as directory:
        work = Path(directory)
        fixture = work / "sample disk"
        (fixture / "alpha/nested").mkdir(parents=True)
        (fixture / "beta").mkdir()
        (fixture / "alpha/a.txt").write_bytes(b"a" * 65536)
        (fixture / "alpha/nested/deep.bin").write_bytes(b"d" * 16384)
        (fixture / "beta/b.bin").write_bytes(b"b" * 131072)
        for index in range(80):
            (fixture / f"row-{index:03}.bin").write_bytes(b"r" * 1024)
        expected = {p.relative_to(fixture).as_posix(): p.stat().st_size
                    for p in fixture.rglob("*") if p.is_file()}
        checks = [
            ("interactions", "--smoke-interactions", "[interaction-smoke] passed:"),
            ("scan-reset", "--smoke-scan-reset", "[scan-reset-smoke] passed:"),
        ]
        for name, flag, marker in checks:
            stage = work / name
            stage.mkdir()
            env = os.environ.copy()
            env["WIZTREE_STATE_DIR"] = str(stage / "settings")
            env["WIZTREE_SMOKE_OUTPUT"] = str(stage)
            env["WIZTREE_DIAGNOSTICS"] = "1"
            env["RUST_BACKTRACE"] = "1"
            result = subprocess.run([str(executable), str(fixture), flag, "--workers=4"],
                                    env=env, capture_output=True, timeout=60)
            log = (result.stdout + result.stderr).decode("utf-8", errors="replace")
            if result.returncode != 0 or marker not in log:
                raise RuntimeError(f"Native desktop check {name} failed (exit {result.returncode}):\n{log[-8000:]}")
            actual = {p.relative_to(fixture).as_posix(): p.stat().st_size
                      for p in fixture.rglob("*") if p.is_file()}
            if actual != expected:
                raise RuntimeError("Desktop acceptance modified fixture contents")
            print(f"native_desktop_check={name} passed", flush=True)
    report = {"platform": sys.platform, "passed": True, "checks": [c[0] for c in checks],
              "production_gpui_event_routing": True, "fixture_unchanged": True,
              "os_mouse_automation": False, "native_file_picker_clicks_validated": False,
              "system_trash_mutation_validated": False, "cold_scan_performance_validated": False}
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    verify(args.binary.resolve(), args.report.resolve())


if __name__ == "__main__":
    main()
