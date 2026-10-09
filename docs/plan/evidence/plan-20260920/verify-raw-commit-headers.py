#!/usr/bin/env python3
"""Verify Libra commit headers without cat-file -p's display filtering."""
from __future__ import annotations

import re
import subprocess
import sys


def main() -> int:
    if len(sys.argv) > 2:
        print("usage: verify-raw-commit-headers.py [commit-sha]", file=sys.stderr)
        return 2
    sha = sys.argv[1] if len(sys.argv) == 2 else subprocess.check_output(
        ["libra", "rev-parse", "HEAD"], text=True
    ).strip()
    batch = subprocess.check_output(
        ["libra", "cat-file", "--batch"], input=(sha + "\n").encode("ascii")
    )
    header, payload = batch.split(b"\n", 1)
    fields = header.split(b" ")
    if len(fields) != 3 or fields[0].decode("ascii") != sha or fields[1] != b"commit":
        print("raw object is not the requested commit", file=sys.stderr)
        return 1
    size = int(fields[2])
    raw = payload[:size]
    if len(raw) != size or b"\n\n" not in raw:
        print("truncated or malformed raw commit", file=sys.stderr)
        return 1
    headers, message = raw.split(b"\n\n", 1)
    header_lines = headers.splitlines()
    committer = re.search(rb"(?m)^committer .+ <([^<>]+)> [0-9]+ [+-][0-9]{4}$", headers)
    trailer_block = message.rstrip(b"\n").rsplit(b"\n\n", 1)[-1].splitlines()
    signoff_emails = [
        match.group(1)
        for line in trailer_block
        if (match := re.fullmatch(rb"Signed-off-by: .+ <([^<>]+)>", line))
    ]
    committer_email = committer.group(1) if committer else None
    has_matching_signoff = committer_email is not None and committer_email in signoff_emails
    has_gpgsig = any(line.startswith(b"gpgsig ") for line in header_lines)
    if not has_matching_signoff or not has_gpgsig:
        print(
            f"commit headers invalid: Signed-off-by-matches-committer={str(has_matching_signoff).lower()} "
            f"gpgsig={str(has_gpgsig).lower()}",
            file=sys.stderr,
        )
        return 1
    print(f"commit_sha={sha} Signed-off-by-matches-committer=true gpgsig=true")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
