#!/usr/bin/env python3
"""Check that every Quant target belongs to one application and Cargo workspace."""

import re
import subprocess
import tomllib
from pathlib import Path


def main():
    root = Path(__file__).resolve().parent.parent
    tracked = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard"], cwd=root, text=True
    ).splitlines()
    paths = {path for path in tracked if (root / path).is_file()}
    assert {p for p in paths if Path(p).name == "leptoskit.toml"} == {"leptoskit.toml"}
    assert {p for p in paths if Path(p).name == "Cargo.lock"} == {"Cargo.lock"}
    assert "leptoskit.workspace.toml" not in paths
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    packages = {}
    for path in paths:
        if Path(path).name != "Cargo.toml" or path == "Cargo.toml":
            continue
        member = tomllib.loads((root / path).read_text())
        assert "workspace" not in member, f"Nested workspace: {path}"
        packages[member["package"]["name"]] = Path(path).parent
    config = tomllib.loads((root / "leptoskit.toml").read_text())
    for frontend in config["frontends"].values():
        assert frontend["package"] in packages
        if "server_package" in frontend:
            assert frontend["server_package"] in packages
        for directory in frontend.get("inputs", {}).values():
            assert not Path(directory).is_absolute() and ".." not in Path(directory).parts
    for target in config["targets"].values():
        if "frontend" in target:
            assert target["frontend"] in config["frontends"]
        if "build" in target and "server_package" in target["build"]:
            assert target["build"]["server_package"] in packages
    sdk = "https://github.com/leptoskit/leptoskit"
    pins = {
        value["rev"]
        for section in (manifest["workspace"]["dependencies"], manifest["patch"]["crates-io"])
        for value in section.values()
        if isinstance(value, dict) and value.get("git") == sdk
    }
    assert len(pins) == 1, f"SDK pins disagree: {pins}"
    revision = pins.pop()
    for name in ("landing", "relay"):
        docker = (root / "deploy" / name / "Dockerfile").read_text()
        assert re.search(rf"(?m)^ARG LEPTOSKIT_REV={revision}$", docker)
    print(f"One application, {len(packages)} Cargo packages, {len(config['targets'])} named targets; SDK {revision}.")


if __name__ == "__main__":
    main()
