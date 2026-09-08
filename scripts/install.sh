#!/usr/bin/env sh
set -eu

REPOSITORY="${ANUREO_REPO:-anureo/anureo}"
VERSION="${ANUREO_VERSION:-latest}"
INSTALL_DIR="${ANUREO_INSTALL_DIR:-$HOME/.local/bin}"
BETA="${ANUREO_BETA:-}"

usage() {
    cat <<'EOF'
Install anureo from GitHub Releases.

Usage:
  ./install.sh [--beta] [--version VERSION] [--install-dir DIR] [--repo OWNER/REPO]

Options:
  --beta               Install the latest beta (pre-release) instead of the latest stable

Environment variables:
  ANUREO_BETA          Set to 1 to install the latest beta release (same as --beta)
  ANUREO_VERSION       Release tag without the leading v (default: latest)
  ANUREO_INSTALL_DIR   Installation directory (default: ~/.local/bin)
  ANUREO_REPO          GitHub repository (default: anureo/anureo)
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || { echo "missing value for --version" >&2; exit 2; }
            VERSION="$2"
            shift 2
            ;;
        --install-dir)
            [ "$#" -ge 2 ] || { echo "missing value for --install-dir" >&2; exit 2; }
            INSTALL_DIR="$2"
            shift 2
            ;;
        --repo)
            [ "$#" -ge 2 ] || { echo "missing value for --repo" >&2; exit 2; }
            REPOSITORY="$2"
            shift 2
            ;;
        --beta)
            BETA=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS/$ARCH" in
    Linux/x86_64|Linux/amd64) TARGET="x86_64-unknown-linux-gnu" ;;
    Darwin/x86_64|Darwin/amd64) TARGET="x86_64-apple-darwin" ;;
    Darwin/arm64|Darwin/aarch64) TARGET="aarch64-apple-darwin" ;;
    *)
        echo "unsupported platform: $OS/$ARCH" >&2
        exit 1
        ;;
esac

command -v curl >/dev/null 2>&1 || {
    echo "curl is required to install anureo" >&2
    exit 1
}
command -v tar >/dev/null 2>&1 || {
    echo "tar is required to install anureo" >&2
    exit 1
}

if [ "$VERSION" = "latest" ] && [ -n "$BETA" ]; then
    # Beta releases are published as GitHub pre-releases, which the plain
    # "latest" resolution below never selects. List releases and take the
    # newest tag carrying a pre-release suffix (e.g. v0.6.0-beta).
    echo "Looking up the latest anureo beta release..."
    RELEASES_JSON="$(curl -fsSL "https://api.github.com/repos/$REPOSITORY/releases?per_page=100")" || {
        echo "could not list releases from GitHub" >&2
        exit 1
    }
    TAG="$(printf '%s\n' "$RELEASES_JSON" | sed -n 's/.*"tag_name": *"\(v[0-9][0-9.]*-[^"]*\)".*/\1/p' | head -n 1)"
    [ -n "$TAG" ] || { echo "no anureo beta release found" >&2; exit 1; }
    VERSION="${TAG#v}"
    echo "Installing anureo pre-release $VERSION"
fi

if [ "$VERSION" = "latest" ]; then
    RELEASE_URL="https://github.com/$REPOSITORY/releases/latest/download"
else
    VERSION="${VERSION#v}"
    RELEASE_URL="https://github.com/$REPOSITORY/releases/download/v$VERSION"
fi

ARCHIVE="anureo-${VERSION}-${TARGET}.tar.gz"
if [ "$VERSION" = "latest" ]; then
    ARCHIVE="anureo-latest-${TARGET}.tar.gz"
    # GitHub's latest-download URL uses the actual release version in the
    # filename, so resolve the tag before downloading the archive.
    TAG="$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPOSITORY/releases/latest" | sed -n 's#.*/tag/##p')"
    [ -n "$TAG" ] || { echo "could not determine the latest anureo release" >&2; exit 1; }
    VERSION="${TAG#v}"
    RELEASE_URL="https://github.com/$REPOSITORY/releases/download/$TAG"
    ARCHIVE="anureo-${VERSION}-${TARGET}.tar.gz"
fi

TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t anureo-install)"
cleanup() { rm -rf "$TMP_DIR"; }
trap cleanup EXIT INT TERM

ARCHIVE_PATH="$TMP_DIR/$ARCHIVE"
echo "Downloading anureo $VERSION for $TARGET..."
curl -fL --retry 3 --proto '=https' --tlsv1.2 \
    "$RELEASE_URL/$ARCHIVE" -o "$ARCHIVE_PATH"

mkdir -p "$INSTALL_DIR"
tar -xzf "$ARCHIVE_PATH" -C "$TMP_DIR"
[ -f "$TMP_DIR/anureo" ] || { echo "release archive does not contain anureo" >&2; exit 1; }
chmod 755 "$TMP_DIR/anureo"
INSTALL_NAME="anureo"
if [ -n "$BETA" ]; then
    INSTALL_NAME="anureo-beta"
fi
mv "$TMP_DIR/anureo" "$INSTALL_DIR/$INSTALL_NAME"

echo "anureo installed to $INSTALL_DIR/$INSTALL_NAME"
case ":${PATH:-}:" in
    *:"$INSTALL_DIR":*) ;;
    *) echo "Add $INSTALL_DIR to PATH to run: $INSTALL_NAME" ;;
esac
