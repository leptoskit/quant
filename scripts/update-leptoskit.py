#!/usr/bin/env python3
"""Pin both apps and the relay build CLI to the current Leptoskit main."""

import re
import subprocess
from pathlib import Path


def main():
    root = Path(__file__).resolve().parent.parent
    repository = "https://github.com/leptoskit/leptoskit"
    result = subprocess.check_output(
        ["git", "ls-remote", "--exit-code", repository, "refs/heads/main"],
        text=True,
    )
    revision = result.split()[0]
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise SystemExit(f"Unexpected main revision: {revision!r}")

    manifests = [root / "apps" / app / "Cargo.toml" for app in ("quant", "relay")]
    updates = {}
    for manifest in manifests:
        # A local patch can hide compatibility problems and produce a lockfile
        # that other machines cannot reproduce.
        config = manifest.parent / ".cargo" / "config.toml"
        if config.exists() and repository in config.read_text():
            raise SystemExit(f"Disable the local SDK override in {config} first")
        contents, count = re.subn(
            r'(git = "' + re.escape(repository) + r'", rev = ")[0-9a-f]{40}(")',
            lambda match: match[1] + revision + match[2],
            manifest.read_text(),
        )
        if not count:
            raise SystemExit(f"No Leptoskit revision pins found in {manifest}")
        updates[manifest] = contents

    dockerfile = root / "apps/relay/Dockerfile"
    contents, count = re.subn(
        r"(?m)^ARG LEPTOSKIT_REV=[0-9a-f]{40}$",
        f"ARG LEPTOSKIT_REV={revision}",
        dockerfile.read_text(),
    )
    if count != 1:
        raise SystemExit("Expected one LEPTOSKIT_REV in the relay Dockerfile")
    updates[dockerfile] = contents
    for path, contents in updates.items():
        path.write_text(contents)

    print(f"Pinned Leptoskit main to {revision}", flush=True)
    for manifest in manifests:
        command = ["cargo", "check", "--workspace", "--all-targets"]
        if manifest.parent.name == "quant":
            # The app-owned host includes a registrant written by the matching
            # CLI during `leptoskit build`; it is absent on a fresh checkout.
            command.extend(["--exclude", "quant_macos"])
        subprocess.run(
            command,
            cwd=manifest.parent,
            check=True,
        )
    print("Lockfiles updated; Rust checks passed (generated macOS host excluded).")
    print("Run app tests and build the native host with the matching CLI before release.")


if __name__ == "__main__":
    main()
