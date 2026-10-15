"""Verify the actual native package before it can become a release asset."""
import argparse
import csv
import ctypes
import io
import tarfile
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import subprocess
import sys
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
    elif args.platform == "windows":
        with zipfile.ZipFile(args.out / (stem + ".zip")) as archive:
            for item in archive.infolist():
                parts = Path(item.filename)
                if parts.is_absolute() or ".." in parts.parts or ":" in item.filename or "\\" in item.filename:
                    raise RuntimeError("Unsafe package archive path")
            archive.extractall(work)
        folder = work / stem
        executable = folder / "WizTree.exe"
        same_binary(executable, args.binary_dir / "wiztree-rust.exe")
        if list(folder.rglob("disk-scan*")):
            raise RuntimeError("Developer scanner must not be shipped in the desktop package")
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.LoadLibraryExW.argtypes = [ctypes.c_wchar_p, ctypes.c_void_p, ctypes.c_uint32]
        kernel.LoadLibraryExW.restype = ctypes.c_void_p
        kernel.FindResourceW.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p]
        kernel.FindResourceW.restype = ctypes.c_void_p
        kernel.FreeLibrary.argtypes = [ctypes.c_void_p]
        module = kernel.LoadLibraryExW(str(executable), None, 0x22)
        if not module:
            raise RuntimeError("Cannot read Windows executable resources")
        try:
            if not kernel.FindResourceW(module, 1, 14):
                raise RuntimeError("Missing Windows application icon resource #1")
        finally:
            kernel.FreeLibrary(module)
        report["embedded_windows_icon_validated"] = True
        scanner = args.binary_dir / "disk-scan.exe"
        report["scanner_scope"] = "native build (desktop package does not ship the CLI)"
    else:
        deb_arch = {"x86_64": "amd64", "arm64": "arm64"}[args.arch]
        package = args.out / f"WizTree-{version}-ubuntu-{deb_arch}.deb"
        control = run(["dpkg-deb", "--field", package]).stdout
        fields = dict(line.split(": ", 1) for line in control.splitlines() if ": " in line and not line.startswith(" "))
        if (fields.get("Package"), fields.get("Version"), fields.get("Architecture")) != ("wiztree", version, deb_arch):
            raise RuntimeError("Incorrect DEB package identity")
        if not fields.get("Depends") or "libc6" not in fields["Depends"]:
            raise RuntimeError("Missing runtime dependencies")
        raw = subprocess.check_output(["dpkg-deb", "--fsys-tarfile", str(package)], timeout=90)
        with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
            for member in archive.getmembers():
                relative = Path(member.name)
                if relative.is_absolute() or ".." in relative.parts or member.uid != 0 or member.gid != 0:
                    raise RuntimeError("Unsafe DEB layout or non-neutral owner identity")
                if not (member.isfile() or member.isdir()):
                    raise RuntimeError("Unexpected DEB entry type")
        folder = work / "deb-root"
        run(["dpkg-deb", "--extract", package, folder])
        executable = folder / "usr/bin/wiztree"
        same_binary(executable, args.binary_dir / "wiztree-rust")
        if list(folder.rglob("disk-scan*")) or list(folder.rglob("install.sh")):
            raise RuntimeError("Desktop DEB must not ship the developer CLI or an install script")
        launcher = folder / "usr/share/applications/app.wiztree.desktop.desktop"
        run(["desktop-file-validate", launcher])
        text = launcher.read_text(encoding="utf-8")
        for line in ("Exec=wiztree", "Icon=app.wiztree.desktop", "StartupWMClass=app.wiztree.desktop"):
            if line not in text.splitlines():
                raise RuntimeError("Incorrect Ubuntu application launcher")
        same_binary(folder / "usr/share/icons/hicolor/256x256/apps/app.wiztree.desktop.png",
                    ROOT / "crates/desktop/assets/WizTree.png")
        missing = run(["ldd", executable]).stdout
        if "not found" in missing:
            raise RuntimeError("Missing native runtime library")
        from verify_release import audit
        if not audit(folder)["passed"]:
            raise RuntimeError("DEB contents failed the release privacy audit")
        report["deb_layout_dependencies_and_icons_validated"] = True
        report["package_manager_install_upgrade_purge_validated"] = False
        if args.install_check:
            script = """set -eu
apt-get update -qq
apt-get install -y --no-install-recommends /packages/*.deb
[ "$(dpkg-query -W -f='${Status}' wiztree)" = 'install ok installed' ]
test -x /usr/bin/wiztree
test -f /usr/share/applications/app.wiztree.desktop.desktop
test -f /usr/share/icons/hicolor/256x256/apps/app.wiztree.desktop.png
test ! -e /usr/bin/disk-scan
ldd /usr/bin/wiztree > /tmp/wiztree-libraries
! grep -q 'not found' /tmp/wiztree-libraries
dpkg -i /packages/*.deb
apt-get purge -y wiztree
test ! -e /usr/bin/wiztree
test ! -e /usr/share/applications/app.wiztree.desktop.desktop
test ! -e /usr/share/icons/hicolor/256x256/apps/app.wiztree.desktop.png
"""
            subprocess.run(["docker", "run", "--rm", "-v", f"{args.out}:/packages:ro",
                            "ubuntu:24.04", "bash", "-c", script], check=True, timeout=360)
            report["package_manager_install_upgrade_purge_validated"] = True
        scanner = args.binary_dir / "disk-scan"
        report["scanner_scope"] = "native build (desktop DEB does not ship the CLI)"
    report["scanner"] = verify_scanner(scanner, work)
    report["passed"] = True
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--install-check", action="store_true", help="Validate Ubuntu package installation/reinstall/purge in a disposable container")
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
