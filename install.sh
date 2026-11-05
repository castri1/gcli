#!/bin/sh
# Installs gcli from a GitHub release (macOS and Linux).
#
#   curl -fsSL https://raw.githubusercontent.com/castri1/gcli/main/install.sh | sh
#
# Environment variables:
#   GCLI_VERSION       release tag to install (default: the latest), e.g. v0.1.0
#   GCLI_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
set -eu

REPO="castri1/gcli"
VERSION="${GCLI_VERSION:-latest}"
INSTALL_DIR="${GCLI_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
    echo "gcli: $*" >&2
    exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required"

case "$(uname -s)" in
    Darwin) os="apple-darwin" ;;
    Linux) os="unknown-linux-musl" ;;
    *) fail "unsupported system $(uname -s); on Windows use install.ps1" ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *) fail "unsupported architecture $(uname -m)" ;;
esac
target="$arch-$os"

if [ "$VERSION" = "latest" ]; then
    # /releases/latest redirects to /releases/tag/<tag>; no API call, no rate limit.
    VERSION=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's|.*/||')
    case "$VERSION" in
        v*) ;;
        *) fail "no release found in github.com/$REPO" ;;
    esac
fi

name="gcli-$VERSION-$target"
url="https://github.com/$REPO/releases/download/$VERSION/$name.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading gcli $VERSION ($target)…"
curl -fsSL "$url" -o "$tmp/$name.tar.gz" || fail "download failed: $url"
curl -fsSL "$url.sha256" -o "$tmp/$name.tar.gz.sha256" || fail "download failed: $url.sha256"
if command -v shasum >/dev/null 2>&1; then
    (cd "$tmp" && shasum -a 256 -c "$name.tar.gz.sha256" >/dev/null 2>&1) || fail "checksum mismatch"
else
    (cd "$tmp" && sha256sum -c "$name.tar.gz.sha256" >/dev/null 2>&1) || fail "checksum mismatch"
fi

tar xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 755 "$tmp/$name/gcli" "$INSTALL_DIR/gcli"
echo "✓ Installed $INSTALL_DIR/gcli ($("$INSTALL_DIR/gcli" --version))"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        case "$(basename "${SHELL:-sh}")" in
            zsh) rc="$HOME/.zshrc" ;;
            bash) rc="$HOME/.bashrc" ;;
            *) rc="your shell's startup file" ;;
        esac
        echo
        echo "$INSTALL_DIR is not on your PATH. Add this line to $rc and open a new terminal:"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac

if ! command -v gcloud >/dev/null 2>&1; then
    echo
    echo "Note: signing in to Google Cloud uses the gcloud CLI:"
    echo "  https://cloud.google.com/sdk/docs/install"
fi
