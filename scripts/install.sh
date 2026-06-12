#!/bin/sh
# devflow installer — downloads the latest release binary from GitHub,
# verifies its SHA-256 checksum, and installs it.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/clement-tourriere/devflow/main/scripts/install.sh | sh
#
# Options (environment variables, or a version as the first argument):
#   DEVFLOW_VERSION      Release tag to install (e.g. v0.5.0). Default: latest
#   DEVFLOW_INSTALL_DIR  Install directory. Default: ~/.local/bin
#
# Once installed, upgrade with: devflow update

set -eu

REPO="clement-tourriere/devflow"

say() { printf '%s\n' "$1"; }
err() {
    printf 'devflow install: %s\n' "$1" >&2
    exit 1
}

download() {
    url="$1"
    dest="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$dest" "$url" \
            || err "download failed: $url"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$dest" "$url" || err "download failed: $url"
    else
        err "curl or wget is required"
    fi
}

main() {
    version="${1:-${DEVFLOW_VERSION:-latest}}"
    install_dir="${DEVFLOW_INSTALL_DIR:-$HOME/.local/bin}"

    # Map OS/arch to a release asset name.
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux)
            case "$arch" in
                x86_64 | amd64) asset="devflow-linux-amd64" ;;
                aarch64 | arm64) asset="devflow-linux-arm64" ;;
                *) err "unsupported Linux architecture: $arch (install from source: cargo install --git https://github.com/$REPO)" ;;
            esac
            ;;
        Darwin)
            case "$arch" in
                arm64) asset="devflow-macos-arm64" ;;
                *) err "unsupported macOS architecture: $arch — Apple Silicon only (install from source: cargo install --git https://github.com/$REPO)" ;;
            esac
            ;;
        *)
            err "unsupported operating system: $os (install from source: cargo install --git https://github.com/$REPO)"
            ;;
    esac

    if [ "$version" = "latest" ]; then
        base_url="https://github.com/$REPO/releases/latest/download"
    else
        # Accept both "0.5.0" and "v0.5.0".
        case "$version" in
            v*) ;;
            *) version="v$version" ;;
        esac
        base_url="https://github.com/$REPO/releases/download/$version"
    fi

    tmp_dir="$(mktemp -d)"
    trap 'rm -rf "$tmp_dir"' EXIT INT TERM

    say "Downloading $asset ($version)..."
    download "$base_url/$asset" "$tmp_dir/devflow"
    download "$base_url/$asset.sha256" "$tmp_dir/devflow.sha256"

    # Verify checksum.
    expected="$(awk '{print $1}' "$tmp_dir/devflow.sha256")"
    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$tmp_dir/devflow" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        actual="$(shasum -a 256 "$tmp_dir/devflow" | awk '{print $1}')"
    else
        err "neither sha256sum nor shasum found; cannot verify the download"
    fi
    if [ "$expected" != "$actual" ]; then
        err "checksum mismatch for $asset (expected $expected, got $actual)"
    fi

    chmod +x "$tmp_dir/devflow"
    "$tmp_dir/devflow" --version >/dev/null 2>&1 \
        || err "downloaded binary failed to run"

    # Stage inside the install dir, then rename: atomic, and safe to replace
    # a currently running devflow (avoids ETXTBSY on Linux).
    mkdir -p "$install_dir" || err "cannot create $install_dir"
    staged="$install_dir/.devflow-install.$$"
    cp "$tmp_dir/devflow" "$staged" || err "cannot write to $install_dir"
    mv -f "$staged" "$install_dir/devflow"

    installed_version="$("$install_dir/devflow" --version)"
    say "Installed $installed_version to $install_dir/devflow"

    case ":$PATH:" in
        *":$install_dir:"*) ;;
        *)
            say ""
            say "NOTE: $install_dir is not on your PATH. Add this to your shell profile:"
            say "  export PATH=\"$install_dir:\$PATH\""
            ;;
    esac

    say ""
    say "Get started:  devflow init"
    say "Stay updated: devflow update"
}

main "$@"
