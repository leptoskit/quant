#!/usr/bin/env python3
"""Pin Quant's SDK dependencies and build CLIs to the latest Leptoskit main."""

import argparse
import re
import subprocess
from pathlib import Path

REPOSITORY = "https://github.com/leptoskit/leptoskit"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-check", action="store_true", help="Resolve the lockfile; leave compilation to CI.")
    arguments = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    config = root / ".cargo/config.toml"
    if config.exists() and REPOSITORY in config.read_text():
        raise SystemExit(f"Disable the local SDK override in {config} first")
    revision = subprocess.check_output(
        ["git", "ls-remote", "--exit-code", REPOSITORY, "refs/heads/main"], text=True
    ).split()[0]
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise SystemExit(f"Unexpected main revision: {revision!r}")
    manifest = root / "Cargo.toml"
    contents, count = re.subn(
        r'(git = "' + re.escape(REPOSITORY) + r'", rev = ")[0-9a-f]{40}(")',
        lambda match: match[1] + revision + match[2], manifest.read_text()
    )
    if not count:
        raise SystemExit("No SDK pins found in the root manifest")
    updates = {manifest: contents}
    for name in ("landing", "relay"):
        dockerfile = root / "deploy" / name / "Dockerfile"
        contents, count = re.subn(r"(?m)^ARG LEPTOSKIT_REV=[0-9a-f]{40}$", f"ARG LEPTOSKIT_REV={revision}", dockerfile.read_text())
        if count != 1:
            raise SystemExit(f"Expected one SDK revision in {dockerfile}")
        updates[dockerfile] = contents
    for path, contents in updates.items():
        path.write_text(contents)
    print(f"Pinned Leptoskit main to {revision}", flush=True)
    subprocess.run(["cargo", "update", "--workspace"], cwd=root, check=True)
    if not arguments.no_check:
        # Platform features remain separate, including in a single workspace.
        for packages in (
            (),  # Core default members.
            ("quant_app", "quant_model", "quant_service"),
            ("relay", "relay_db"),
            ("landing", "landing_app", "landing_backend", "landing_shared"),
        ):
            command = ["cargo", "check", "--locked", "--all-targets"]
            for package in packages:
                command.extend(["-p", package])
            subprocess.run(command, cwd=root, check=True)
    print("Updated the application's single lockfile. Native host generation uses the matching CLI.")


if __name__ == "__main__":
    main()
