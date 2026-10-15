"""Build distributable binaries without developer-machine source paths."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]

def build(target=None):
    env = os.environ.copy()
    cargo = env.get("CARGO", "cargo")
    if target is None and sys.platform == "win32":
        rustc = env.get("RUSTC", str(Path(cargo).with_name("rustc.exe")))
        target = next(line.split(": ",1)[1] for line in subprocess.check_output([rustc,"-vV"],text=True).splitlines() if line.startswith("host: "))
    roots = {Path.home(): "/build/home", ROOT: "/build/project"}
    for key, default, replacement in [
        ("CARGO_HOME", Path.home()/".cargo", "/build/cargo"),
        ("RUSTUP_HOME", Path.home()/".rustup", "/build/toolchain"),
    ]:
        roots[Path(env.get(key, str(default))).resolve()] = replacement
    flags = []
    for source, dest in sorted(roots.items(), key=lambda pair: len(str(pair[0]))):
        flags.append(f"--remap-path-prefix={source}={dest}")
        if os.name == "nt":
            flags.append(f"--remap-path-prefix={source.as_posix()}={dest}")
    if (target and target.endswith("windows-msvc")) or (not target and sys.platform == "win32"):
        flags += ["-C", "link-arg=/PDBALTPATH:WizTree.pdb", "-C", "target-feature=+crt-static"]
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    # The Metal compiler embeds OUT_DIR separately from rustc path remapping.
    build_root = (Path(tempfile.mkdtemp(prefix="wiztree-build-", dir="/private/tmp"))
                  if sys.platform == "darwin" else ROOT/"target"/"distributable")
    env["CARGO_TARGET_DIR"] = str(build_root)
    if sys.platform == "darwin":
        env["MACOSX_DEPLOYMENT_TARGET"] = "14.0"
    env["CARGO_PROFILE_RELEASE_DEBUG"] = "0"
    env["CARGO_PROFILE_RELEASE_STRIP"] = "symbols"
    command = [cargo, "build", "--locked", "--release", "--workspace"]
    if target:
        command += ["--target", target]
    subprocess.run(command, cwd=ROOT, env=env, check=True)
    return build_root/(target or "")/"release"

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target")
    args = parser.parse_args()
    print(build(args.target))
