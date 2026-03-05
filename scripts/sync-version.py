#!/usr/bin/env python3

import re
import sys
from pathlib import Path


TARGET_PACKAGES = {
    "devflow",
    "devflow-app",
    "devflow-core",
    "devflow-proxy",
    "devflow-terminal",
}


def validate_version(version: str) -> None:
    if not re.fullmatch(
        r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version
    ):
        raise ValueError(f"Invalid semver version: {version}")


def update_package_version_toml(file_path: Path, new_version: str) -> bool:
    lines = file_path.read_text(encoding="utf-8").splitlines(keepends=True)

    in_package = False
    found = False
    changed = False

    for index, line in enumerate(lines):
        stripped = line.strip()

        if stripped == "[package]":
            in_package = True
            continue

        if in_package and stripped.startswith("["):
            in_package = False

        if in_package and stripped.startswith("version"):
            match = re.match(r'^(\s*version\s*=\s*")([^"]+)("\s*)$', line)
            if not match:
                raise ValueError(f"Failed to parse version line in {file_path}")

            found = True
            if match.group(2) != new_version:
                lines[index] = f"{match.group(1)}{new_version}{match.group(3)}"
                changed = True
            break

    if not found:
        raise ValueError(f"Could not find [package] version in {file_path}")

    if changed:
        file_path.write_text("".join(lines), encoding="utf-8")

    return changed


def update_json_version(file_path: Path, new_version: str) -> bool:
    text = file_path.read_text(encoding="utf-8")
    updated, count = re.subn(
        r'("version"\s*:\s*")[^"]+(")',
        rf"\g<1>{new_version}\2",
        text,
        count=1,
    )
    if count != 1:
        raise ValueError(f"Could not update JSON version in {file_path}")
    file_path.write_text(updated, encoding="utf-8")
    return True


def update_docs_version(file_path: Path, new_version: str) -> bool:
    text = file_path.read_text(encoding="utf-8")

    updated = text
    updated, count_a = re.subn(
        r'(class="version">v)([0-9A-Za-z.+\-]+)',
        rf"\g<1>{new_version}",
        updated,
        count=1,
    )
    updated, count_b = re.subn(
        r"(<strong>devflow</strong>\s+v)([0-9A-Za-z.+\-]+)",
        rf"\g<1>{new_version}",
        updated,
        count=1,
    )

    if count_a != 1 or count_b != 1:
        raise ValueError(f"Could not update docs version markers in {file_path}")

    file_path.write_text(updated, encoding="utf-8")
    return True


def update_llms_version(file_path: Path, new_version: str) -> bool:
    text = file_path.read_text(encoding="utf-8")
    updated, count = re.subn(
        r"(^> Version:\s+)([^\n]+)$",
        rf"\g<1>{new_version}",
        text,
        count=1,
        flags=re.MULTILINE,
    )
    if count != 1:
        raise ValueError(f"Could not update llms version in {file_path}")
    file_path.write_text(updated, encoding="utf-8")
    return True


def update_cargo_lock(file_path: Path, new_version: str) -> bool:
    text = file_path.read_text(encoding="utf-8")

    pattern = re.compile(
        r'(?m)^(\[\[package\]\]\nname = "(?P<name>devflow|devflow-app|devflow-core|devflow-proxy|devflow-terminal)"\nversion = ")(?P<version>[^"]+)(")'
    )

    def repl(match: re.Match[str]) -> str:
        name = match.group("name")
        if name not in TARGET_PACKAGES:
            return match.group(0)
        return f"{match.group(1)}{new_version}{match.group(4)}"

    updated, count = pattern.subn(repl, text)
    if count != len(TARGET_PACKAGES):
        raise ValueError(
            f"Expected {len(TARGET_PACKAGES)} devflow package entries in {file_path}, found {count}"
        )

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

    toml_files = [
        root / "Cargo.toml",
        root / "crates/devflow-core/Cargo.toml",
        root / "crates/devflow-proxy/Cargo.toml",
        root / "crates/devflow-terminal/Cargo.toml",
        root / "src-tauri/Cargo.toml",
    ]

    for file_path in toml_files:
        update_package_version_toml(file_path, new_version)

    update_json_version(root / "src-tauri/tauri.conf.json", new_version)
    update_json_version(root / "ui/package.json", new_version)
    update_docs_version(root / "docs/index.html", new_version)
    update_llms_version(root / "llms-full.txt", new_version)
    update_cargo_lock(root / "Cargo.lock", new_version)

    print(f"Synchronized project version to {new_version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
