"""Create allowlisted source archives and native platform packages."""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile

from verify_release import audit
from release_icons import iconset

ROOT=Path(__file__).resolve().parents[1]
PUBLIC=ROOT/"packaging"/"public"
VERSION=tomllib.loads((ROOT/"Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
RELEASE_TOOLS=["build_release.py","package_release.py","verify_release.py","release_icons.py","verify_package.py","verify_desktop.py"]

def require_clean(path):
    result=audit(path)
    if not result["passed"]:
        raise RuntimeError("Release privacy audit failed: "+json.dumps(result["findings"]))
    return result

def zip_tree(tree, output):
    with zipfile.ZipFile(output,"w",compression=zipfile.ZIP_DEFLATED,compresslevel=9) as archive:
        for path in sorted(tree.rglob("*")):
            if not path.is_file():continue
            if path.is_symlink():raise RuntimeError("Unexpected symlink in release tree")
            info=zipfile.ZipInfo((Path(tree.name)/path.relative_to(tree)).as_posix(),date_time=(1980,1,1,0,0,0))
            info.create_system=3
            mode=0o755 if path.suffix==".sh" else 0o644
            info.external_attr=(stat.S_IFREG|mode)<<16
            info.compress_type=zipfile.ZIP_DEFLATED
            archive.writestr(info,path.read_bytes())

def public_source(stage, out):
    tree=stage/f"WizTree-{VERSION}-source";tree.mkdir()
    for name in ["Cargo.toml","Cargo.lock"]:
        shutil.copyfile(ROOT/name,tree/name)
    for path in (ROOT/"crates").rglob("*"):
        if path.is_file() and (path.suffix==".rs" or path.name=="Cargo.toml"):
            if path.is_symlink():raise RuntimeError("Unexpected source symlink")
            dest=tree/path.relative_to(ROOT);dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(path,dest)
    for path in PUBLIC.rglob("*"):
        if path.is_file() and (path.suffix==".md" or path.name in {"LICENSE",".gitignore","install-linux.sh"}
                               or path.relative_to(PUBLIC)==Path("docs/media/wiztree-preview.png")):
            if path.is_symlink():raise RuntimeError("Unexpected public template symlink")
            relative=path.relative_to(PUBLIC)
            for dest in [tree/relative,tree/"packaging"/"public"/relative]:
                dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(path,dest)
    for name in RELEASE_TOOLS:
        dest=tree/"tools"/name;dest.parent.mkdir(exist_ok=True);shutil.copyfile(ROOT/"tools"/name,dest)
    for name in ["rust.yml","release.yml"]:
        dest=tree/".github"/"workflows"/name;dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(ROOT/".github"/"workflows"/name,dest)
    require_clean(tree)
    output=out/f"WizTree-{VERSION}-source.zip";zip_tree(tree,output);require_clean(output)
    return output,tree

def notices(tree, target):
    cargo=os.environ.get("CARGO","cargo")
    command=[cargo,"metadata","--locked","--offline","--format-version","1","--filter-platform",target]
    metadata=json.loads(subprocess.check_output(command,cwd=ROOT))
    third=tree/"THIRD_PARTY_LICENSES";third.mkdir(parents=True)
    inventory=[]
    for package in sorted(metadata["packages"],key=lambda p:(p["name"],p["version"])):
        if not package.get("source"):continue
        base=Path(package["manifest_path"]).parent
        identity=f"{package['name']}-{package['version']}"
        files=[p for p in base.rglob("*") if p.is_file() and p.name.lower().startswith(("license","licence","copying","copyright","notice","ofl"))]
        for path in files:
            dest=third/identity/path.relative_to(base);dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(path,dest)
        source_url=f"https://crates.io/crates/{package['name']}/{package['version']}"
        inventory.append({"name":package["name"],"version":package["version"],"license":package.get("license"),"repository":package.get("repository"),"source":source_url})
        # Include original crate sources for MPL and for packages that omit a
        # standalone notice; upstream copyright/attribution remains unchanged.
        if "MPL" in (package.get("license") or "") or not files:
            archive=base.parents[2]/"cache"/base.parent.name/(identity+".crate")
            if not archive.is_file():raise RuntimeError("Missing corresponding source archive: "+identity)
            dest=third/"sources"/(identity+".crate");dest.parent.mkdir(exist_ok=True);shutil.copyfile(archive,dest)
    (third/"inventory.json").write_text(json.dumps(inventory,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
    lines=["# Third-party components","","Open-source licenses and required notices are retained without alteration.","Upstream company names and copyright holders identify third-party software, not project affiliation.","","| Component | License | Source |","| --- | --- | --- |"]
    lines += [f"| {p['name']} {p['version']} | {p['license']} | [source]({p['source']}) |" for p in inventory]
    (tree/"THIRD_PARTY_NOTICES.md").write_text("\n".join(lines)+"\n",encoding="utf-8")
    return len(inventory)

def common_docs(tree):
    for name in ["README.md","LICENSE","CHANGELOG.md"]:shutil.copyfile(PUBLIC/name,tree/name)
    shutil.copytree(PUBLIC/"docs",tree/"docs")

def app_bundle(stage, binary_dir, arch):
    app=stage/"WizTree.app";contents=app/"Contents"
    macos=contents/"MacOS";resources=contents/"Resources"
    macos.mkdir(parents=True);resources.mkdir()
    executable=macos/"WizTree";shutil.copyfile(binary_dir/"wiztree-rust",executable);executable.chmod(0o755)
    plist={
        "CFBundleName":"WizTree","CFBundleDisplayName":"WizTree","CFBundleExecutable":"WizTree",
        "CFBundleIdentifier":"app.wiztree.desktop","CFBundlePackageType":"APPL","CFBundleInfoDictionaryVersion":"6.0",
        "CFBundleShortVersionString":VERSION,"CFBundleVersion":VERSION,"CFBundleIconFile":"WizTree.icns",
        "CFBundleDevelopmentRegion":"zh_CN","LSMinimumSystemVersion":"14.0","NSHighResolutionCapable":True,
        "NSSupportsAutomaticGraphicsSwitching":True,"NSHumanReadableCopyright":"WizTree contributors",
    }
    for key in ["NSDesktopFolderUsageDescription","NSDocumentsFolderUsageDescription","NSDownloadsFolderUsageDescription","NSRemovableVolumesUsageDescription","NSNetworkVolumesUsageDescription"]:
        plist[key]="分析您选择的文件夹的磁盘占用情况。"
    (contents/"Info.plist").write_bytes(plistlib.dumps(plist,sort_keys=True))
    common_docs(resources)
    notices(resources,"aarch64-apple-darwin" if arch=="arm64" else "x86_64-apple-darwin")
    icons=stage/"WizTree.iconset";iconset(icons)
    subprocess.run(["iconutil","-c","icns",str(icons),"-o",str(resources/"WizTree.icns")],check=True)
    require_clean(app)
    identity=os.environ.get("MACOS_SIGNING_IDENTITY","-")
    command=["codesign","--force","--sign",identity]
    if identity!="-":command += ["--options","runtime","--timestamp"]
    subprocess.run(command+[str(app)],check=True)
    subprocess.run(["codesign","--verify","--strict",str(app)],check=True)
    return app

def package_macos(stage, out, binary_dir, arch):
    if sys.platform!="darwin":raise RuntimeError("DMG creation requires a Mac; no placeholder DMG is generated")
    app=app_bundle(stage,binary_dir,arch)
    volume=stage/"dmg-volume";volume.mkdir()
    shutil.move(str(app),volume/"WizTree.app")
    (volume/"Applications").symlink_to("/Applications",target_is_directory=True)
    (volume/"安装说明.txt").write_text("将 WizTree.app 拖入 Applications（应用程序），然后从启动台打开 WizTree。\n",encoding="utf-8")
    output=out/f"WizTree-{VERSION}-macos-{arch}.dmg"
    subprocess.run(["hdiutil","create","-volname","WizTree","-srcfolder",str(volume),"-ov","-format","UDZO",str(output)],check=True)
    subprocess.run(["hdiutil","verify",str(output)],check=True)
    profile=os.environ.get("MACOS_NOTARY_PROFILE")
    if profile:
        if os.environ.get("MACOS_SIGNING_IDENTITY","-")=="-":raise RuntimeError("Notarization needs a Developer ID identity")
        subprocess.run(["xcrun","notarytool","submit",str(output),"--keychain-profile",profile,"--wait"],check=True)
        subprocess.run(["xcrun","stapler","staple",str(output)],check=True)
        subprocess.run(["xcrun","stapler","validate",str(output)],check=True)
    return output

def package_linux(stage, out, binary_dir, arch):
    tree=stage/f"WizTree-{VERSION}-linux-{arch}";tree.mkdir()
    for original,public in [("wiztree-rust","wiztree"),("disk-scan","disk-scan")]:
        dest=tree/public;shutil.copyfile(binary_dir/original,dest);dest.chmod(0o755)
    common_docs(tree);notices(tree,"x86_64-unknown-linux-gnu")
    from release_icons import png
    (tree/"WizTree.png").write_bytes(png(256))
    shutil.copyfile(PUBLIC/"install-linux.sh",tree/"install.sh");(tree/"install.sh").chmod(0o755)
    require_clean(tree)
    output=out/(tree.name+".tar.gz")
    with output.open("wb") as raw, gzip.GzipFile(filename="",mode="wb",fileobj=raw,mtime=0) as compressed, tarfile.open(fileobj=compressed,mode="w") as archive:
        for path in sorted(tree.rglob("*")):
            if not path.is_file():continue
            info=archive.gettarinfo(str(path),(Path(tree.name)/path.relative_to(tree)).as_posix());info.uid=info.gid=0;info.uname=info.gname="";info.mtime=0
            with path.open("rb") as data:archive.addfile(info,data)
    return output

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform",choices=["windows","macos","linux"])
    parser.add_argument("--arch",choices=["x86_64","arm64"],default="x86_64")
    parser.add_argument("--binary-dir",type=Path)
    parser.add_argument("--source",action="store_true")
    parser.add_argument("--out",type=Path,default=ROOT/"dist")
    args=parser.parse_args()
    out=args.out.resolve();out.mkdir(parents=True,exist_ok=True)
    stage_root=ROOT/"target"/"package-staging";stage_root.mkdir(parents=True,exist_ok=True)
    stage=Path(tempfile.mkdtemp(prefix="release-",dir=stage_root))
    artifacts=[];report={"version":VERSION,"platform":args.platform,"arch":args.arch,"private_development_files_included":False,"audits":{}}
    if args.source:
        archive,tree=public_source(stage,out);artifacts.append(archive)
        public_tree=out/"public-source"
        if public_tree.exists():
            expected={p.relative_to(tree) for p in tree.rglob("*") if p.is_file()}
            existing={p.relative_to(public_tree) for p in public_tree.rglob("*") if p.is_file()}
            if existing-expected:raise RuntimeError("Public source folder has unexpected files; choose a fresh output directory")
        shutil.copytree(tree,public_tree,dirs_exist_ok=True)
    if args.platform:
        if not args.binary_dir:parser.error("--binary-dir is required for executable packages")
        binary_dir=args.binary_dir.resolve()
        if args.platform=="windows":
            tree=stage/f"WizTree-{VERSION}-windows-{args.arch}";tree.mkdir()
            for original,public in [("wiztree-rust.exe","WizTree.exe"),("disk-scan.exe","disk-scan.exe")]:shutil.copyfile(binary_dir/original,tree/public)
            common_docs(tree);report["third_party_components"]=notices(tree,"x86_64-pc-windows-msvc")
            require_clean(tree);output=out/(tree.name+".zip");zip_tree(tree,output);artifacts.append(output)
        elif args.platform=="macos":artifacts.append(package_macos(stage,out,binary_dir,args.arch))
        else:artifacts.append(package_linux(stage,out,binary_dir,args.arch))
    if not artifacts:parser.error("Choose --source and/or --platform")
    for path in artifacts:
        report["audits"][path.name]=require_clean(path) if path.suffix==".zip" else {"passed":True,"scope":"pre-container app/tree audit"}
    sums=[]
    for path in sorted(out.iterdir()):
        if path.is_file() and (path.suffix in [".zip",".dmg"] or path.name.endswith(".tar.gz")):
            sums.append(hashlib.sha256(path.read_bytes()).hexdigest()+"  "+path.name)
    (out/"SHA256SUMS.txt").write_text("\n".join(sums)+"\n",encoding="utf-8")
    (out/f"release-audit-{args.platform or 'source'}-{args.arch}.json").write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
    print(json.dumps({"artifacts":[p.name for p in artifacts],"audit_passed":True},ensure_ascii=False))

if __name__=="__main__":main()
