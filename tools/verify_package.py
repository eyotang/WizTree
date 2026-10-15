"""Verify the actual native package before it can become a release asset."""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]


def run(command, **kwargs):
    return subprocess.run([str(part) for part in command], check=True,
                          capture_output=True, text=True, encoding="utf-8",
                          errors="replace", timeout=90, **kwargs)


def same_binary(packaged, built):
    if hashlib.sha256(packaged.read_bytes()).digest() != hashlib.sha256(built.read_bytes()).digest():
        raise RuntimeError("Packaged executable differs from the validated build")


def verify_scanner(scanner, work):
    fixture = work / "sample disk"
    (fixture / "目录 space" / "nested").mkdir(parents=True)
    (fixture / "empty").mkdir()
    expected = {}
    for index in range(2051):
        expected[f"entry-{index}.bin"] = index % 23
    expected.update({"目录 space/中文.csv": 17, "目录 space/nested/comma,name": 31,
                     "zero": 0, "without-extension": 7})
    for relative, size in expected.items():
        (fixture / relative).write_bytes(b"x" * size)
    if sys.platform != "win32":
        outside = work / "outside"
        outside.mkdir()
        (outside / "excluded").write_bytes(b"x" * 255)
        (fixture / "external-link").symlink_to(outside, target_is_directory=True)
        (fixture / "目录 space" / "loop").symlink_to(fixture, target_is_directory=True)
    previous = None
    for workers in (1, 4):
        output = work / f"scan-{workers}.csv"
        result = run([scanner, "--scan", fixture, "--method", "directory",
                      "--workers", workers, "--csv", output])
        summary = re.search(r"files=(\d+) dirs=(\d+) errors=(\d+) logical=(\d+)", result.stdout)
        if not summary or tuple(map(int, summary.groups())) != (len(expected), 4, 0, sum(expected.values())):
            raise RuntimeError("Packaged scanner returned incorrect totals")
        actual = {}
        allocations = {}
        prefix = str(fixture.resolve()).removeprefix("\\\\?\\").replace("\\", "/") + "/"
        with output.open(encoding="utf-8", newline="") as stream:
            for row in csv.DictReader(stream):
                if row["type"] != "file":
                    continue
                path = row["path"].removeprefix("\\\\?\\").replace("\\", "/")
                if not path.startswith(prefix):
                    raise RuntimeError("CSV contains a path outside the fixture")
                relative = path[len(prefix):]
                if relative in actual:
                    raise RuntimeError("CSV contains duplicate files")
                actual[relative] = int(row["size"])
                allocations[relative] = int(row["allocated"])
        if actual != expected or (previous is not None and allocations != previous):
            raise RuntimeError("Packaged scanner CSV differs across worker counts or from disk contents")
        previous = allocations
    missing = subprocess.run([str(scanner), "--scan", str(work / "missing")],
                             capture_output=True, timeout=30)
    if missing.returncode == 0:
        raise RuntimeError("Packaged scanner accepted a missing scan root")
    return {"files": len(expected), "directories": 4, "logical_bytes": sum(expected.values()),
            "worker_counts": [1, 4], "csv_matches_disk": True, "missing_root_rejected": True,
            "symbolic_links_checked": sys.platform != "win32"}


def verify(args, work):
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    stem = f"WizTree-{version}-{args.platform}-{args.arch}"
    report = {"platform": args.platform, "arch": args.arch, "version": version,
              "gui_interaction_validated": False, "cold_scan_performance_validated": False}
    if args.platform == "macos":
        if sys.platform != "darwin":
            raise RuntimeError("DMG validation requires macOS")
        mount = work / "mounted"
        mount.mkdir()
        run(["hdiutil", "attach", args.out / (stem + ".dmg"), "-readonly", "-nobrowse", "-mountpoint", mount])
        try:
            app = mount / "WizTree.app"
            plist = plistlib.loads((app / "Contents/Info.plist").read_bytes())
            if (plist["CFBundleIdentifier"] != "app.wiztree.desktop" or
                    plist["CFBundleShortVersionString"] != version or
                    plist["CFBundleExecutable"] != "WizTree"):
                raise RuntimeError("Incorrect application bundle metadata")
            if not (mount / "Applications").is_symlink() or os.readlink(mount / "Applications") != "/Applications":
                raise RuntimeError("Missing Applications installation shortcut")
            executable = app / "Contents/MacOS/WizTree"
            # codesign changes Mach-O bytes: validate architecture and signature instead of a raw hash.
            run(["lipo", executable, "-verify_arch", "arm64" if args.arch == "arm64" else "x86_64"])
            run(["codesign", "--verify", "--strict", app])
            if not (app / "Contents/Resources/WizTree.icns").is_file():
                raise RuntimeError("Missing application icon")
            report["bundle_and_dmg_validated"] = True
        finally:
            run(["hdiutil", "detach", mount])
        report["scanner_scope"] = "native build (macOS application bundle does not ship the CLI)"
        scanner = args.binary_dir / "disk-scan"
    else:
        if args.platform == "windows":
            with zipfile.ZipFile(args.out / (stem + ".zip")) as archive:
                for item in archive.infolist():
                    parts = Path(item.filename)
                    if parts.is_absolute() or ".." in parts.parts or ":" in item.filename or "\\" in item.filename:
                        raise RuntimeError("Unsafe package archive path")
                archive.extractall(work)
            folder = work / stem
            same_binary(folder / "WizTree.exe", args.binary_dir / "wiztree-rust.exe")
            same_binary(folder / "disk-scan.exe", args.binary_dir / "disk-scan.exe")
            scanner = folder / "disk-scan.exe"
        else:
            with tarfile.open(args.out / (stem + ".tar.gz")) as archive:
                archive.extractall(work, filter="data")
            folder = work / stem
            same_binary(folder / "wiztree", args.binary_dir / "wiztree-rust")
            same_binary(folder / "disk-scan", args.binary_dir / "disk-scan")
            env = os.environ.copy()
            data = work / "user data 100%"
            env["XDG_DATA_HOME"] = str(data)
            run(["sh", folder / "install.sh"], env=env)
            same_binary(data / "WizTree/wiztree", args.binary_dir / "wiztree-rust")
            same_binary(data / "WizTree/disk-scan", args.binary_dir / "disk-scan")
            if not (data / "applications/WizTree.desktop").is_file():
                raise RuntimeError("Missing application launcher")
            scanner = data / "WizTree/disk-scan"
            report["isolated_user_install_validated"] = True
        report["scanner_scope"] = "extracted package"
    report["scanner"] = verify_scanner(scanner, work)
    report["passed"] = True
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", required=True, choices=["windows", "macos", "linux"])
    parser.add_argument("--arch", default="x86_64", choices=["x86_64", "arm64"])
    parser.add_argument("--binary-dir", required=True, type=Path)
    parser.add_argument("--out", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    args.binary_dir = args.binary_dir.resolve()
    args.out = args.out.resolve()
    with tempfile.TemporaryDirectory(prefix="wiztree-package-check-") as directory:
        report = verify(args, Path(directory))
    output = args.out / f"package-validation-{args.platform}-{args.arch}.json"
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    main()
