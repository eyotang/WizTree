"""Fail a release audit on local identities, internal URLs, keys, or private files."""
import argparse
import getpass
import json
from pathlib import Path
import re
import zipfile

ROOT = Path(__file__).resolve().parents[1]
SECRET = re.compile(rb"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----|(?:github_pat_|gh[pousr]_)[A-Za-z0-9_]{20,}|AKIA[A-Z0-9]{16}")
INTERNAL = re.compile(rb"https?://[^\s\"<>]*(?:\.corp\b|\.internal\b|192\.168\.\d+\.\d+|10\.\d+\.\d+\.\d+)", re.I)
FORBIDDEN = {".git", ".env", "__pycache__", "rea-functions", "ui-review"}

def private_tokens():
    tokens = {str(ROOT), str(ROOT).replace("\\", "/"), str(Path.home()), str(Path.home()).replace("\\", "/")}
    username = getpass.getuser()
    if len(username) >= 10:
        tokens.add(username)
    return {token for token in tokens if len(token) > 5}

def inspect_data(name, data):
    findings = []
    lower = data.lower()
    for token in private_tokens():
        if token.lower().encode() in lower or token.lower().encode("utf-16le") in lower:
            findings.append({"file": name, "category": "developer_identity_or_path"})
            break
    if SECRET.search(data):
        findings.append({"file": name, "category": "credential"})
    if INTERNAL.search(data):
        findings.append({"file": name, "category": "internal_endpoint"})
    return findings

def audit(path):
    path = Path(path)
    findings = []
    count = 0
    if path.is_file() and path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            for name in archive.namelist():
                if name.endswith("/"):
                    continue
                count += 1
                if FORBIDDEN.intersection(Path(name).parts):
                    findings.append({"file": name, "category": "private_file"})
                findings.extend(inspect_data(name, archive.read(name)))
    else:
        for item in sorted(path.rglob("*")) if path.is_dir() else [path]:
            if not item.is_file():
                continue
            count += 1
            relative = item.relative_to(path).as_posix() if path.is_dir() else item.name
            if FORBIDDEN.intersection(Path(relative).parts):
                findings.append({"file": relative, "category": "private_file"})
            findings.extend(inspect_data(relative, item.read_bytes()))
    return {"passed": not findings, "files_checked": count, "findings": findings}

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    result = audit(args.path)
    text = json.dumps(result, ensure_ascii=False, indent=2)
    if args.report:
        args.report.write_text(text+"\n", encoding="utf-8")
    print(text)
    raise SystemExit(0 if result["passed"] else 1)
