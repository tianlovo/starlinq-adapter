"""Check the public export for excluded artifacts and common credential formats.

This supplements manual review; it is not a proof that arbitrary text is anonymous.
Only tracked/staged files are inspected, so local runtime evidence stays local.
"""

import json
import pathlib
import re
import subprocess
import sys


def git_bytes(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT)


def blocked_path(name):
    parts = pathlib.PurePosixPath(name).parts
    return (
        any(part in {".cache", ".runtime", "__pycache__"} for part in parts)
        or name.startswith("docs/gpt-live-verification/evidence/")
        or name.startswith("docs/gpt-live-verification/live-runs/")
        or name.startswith("docs/live-verification-")
        or pathlib.PurePosixPath(name).suffix.lower()
        in {".png", ".jpg", ".jpeg", ".webp", ".gif", ".mp4", ".db", ".sqlite", ".sqlite3", ".pem", ".pfx", ".key", ".log", ".patch"}
    )


ROOT = pathlib.Path(__file__).resolve().parents[1]
PATTERNS = {
    "private_key": re.compile(rb"-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----"),
    "github_token": re.compile(rb"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,})\b"),
    "aws_access_key": re.compile(rb"\bAKIA[0-9A-Z]{16}\b"),
}

if __name__ == "__main__":
    sys.stdout.reconfigure(encoding="utf-8")
    tracked = [p for p in git_bytes("ls-files", "-z").decode("utf-8").split("\0") if p]
    if not tracked:
        raise SystemExit("No tracked/staged files found; stage the intended export before checking.")
    findings = []
    for name in tracked:
        if blocked_path(name):
            findings.append({"path": name, "reason": "excluded_artifact"})
        data = (ROOT / name).read_bytes()
        for label, pattern in PATTERNS.items():
            if pattern.search(data):
                findings.append({"path": name, "reason": label})
    history_paths = git_bytes("rev-list", "--objects", "--all").decode("utf-8")
    for line in history_paths.splitlines():
        _, separator, name = line.partition(" ")
        if separator and blocked_path(name):
            findings.append({"path": name, "reason": "excluded_artifact_in_history"})
    print(json.dumps({"tracked_files": len(tracked), "findings": findings}, ensure_ascii=False, indent=2))
    raise SystemExit(1 if findings else 0)
