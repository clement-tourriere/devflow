#!/usr/bin/env python3
"""Synchronize the devflow version across Cargo manifests and Cargo.lock.

Usage: scripts/sync-version.py <new-version>

The workspace has three versioned packages, each with its own manifest:

    Cargo.toml                       [package]  devflow       (root, CLI)
    crates/devflow-core/Cargo.toml   [package]  devflow-core
    crates/devflow-gui/Cargo.toml    [package]  devflow-gui

The root manifest also contains `[workspace.package]`, which carries no version
today (the script reads `[package]`). If the root ever moves its version into
`[workspace.package]`, the script updates that table instead.

Files are only rewritten when their content changes, so running the script
with the current version is a no-op.
"""

import re
import sys
from pathlib import Path


TARGET_PACKAGES = {
    "devflow",
    "devflow-core",
    "devflow-gui",
}

# Manifests that own a version, relative to the repository root.
MANIFESTS = [
    "Cargo.toml",
    "crates/devflow-core/Cargo.toml",
    "crates/devflow-gui/Cargo.toml",
]

VERSION_TABLES = {"[package]", "[workspace.package]"}


def validate_version(version: str) -> None:
    if not re.fullmatch(
        r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version
    ):
        raise ValueError(f"Invalid semver version: {version}")


def update_package_version_toml(file_path: Path, new_version: str) -> bool:
    """Set `version = "..."` in `[package]` (or `[workspace.package]`).

    Returns True if the file changed. Raises if no version line is found.
    """
    lines = file_path.read_text(encoding="utf-8").splitlines(keepends=True)

    in_table = False
    found = False
    changed = False

    for index, line in enumerate(lines):
        stripped = line.strip()

        if stripped.startswith("["):
            in_table = stripped in VERSION_TABLES
            continue

        if in_table and re.match(r"version\s*=", stripped):
            match = re.match(r'^(\s*version\s*=\s*")([^"]+)("\s*)$', line.rstrip("\n"))
            if not match:
                raise ValueError(f"Failed to parse version line in {file_path}")

            found = True
            if match.group(2) != new_version:
                newline = "\n" if line.endswith("\n") else ""
                lines[index] = f"{match.group(1)}{new_version}{match.group(3)}{newline}"
                changed = True
            break

    if not found:
        raise ValueError(f"Could not find a package version in {file_path}")

    if changed:
        file_path.write_text("".join(lines), encoding="utf-8")

    return changed


def update_cargo_lock(file_path: Path, new_version: str) -> bool:
    text = file_path.read_text(encoding="utf-8")

    names = "|".join(re.escape(name) for name in sorted(TARGET_PACKAGES))
    pattern = re.compile(
        rf'(?m)^(\[\[package\]\]\nname = "(?P<name>{names})"\nversion = ")(?P<version>[^"]+)(")'
    )

    updated, count = pattern.subn(
        lambda m: f"{m.group(1)}{new_version}{m.group(4)}", text
    )
    if count != len(TARGET_PACKAGES):
        raise ValueError(
            f"Expected {len(TARGET_PACKAGES)} devflow package entries in {file_path}, found {count}"
        )

    if updated == text:
        return False

    file_path.write_text(updated, encoding="utf-8")
    return True


def main() -> int:
    if len(sys.argv) != 2:
        print("Usage: scripts/sync-version.py <new-version>", file=sys.stderr)
        return 2

    new_version = sys.argv[1].strip()

    try:
        validate_version(new_version)
    except ValueError as exc:
        print(str(exc), file=sys.stderr)
        return 2

    root = Path(__file__).resolve().parent.parent

    for relative in MANIFESTS:
        update_package_version_toml(root / relative, new_version)

    update_cargo_lock(root / "Cargo.lock", new_version)

    print(f"Synchronized project version to {new_version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
