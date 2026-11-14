#!/bin/sh
# Install tokenstick as a systemd service.
#
#   sudo ./install.sh                      install from the current release directory
#   sudo ./install.sh --version v0.1.0     download that release from GitHub, then install
#   sudo ./install.sh --uninstall          stop and remove the service (keeps models and config)
#
# Run from an unpacked release tarball (tokenstick binary next to this script's
# parent directory layout) or use --version to download one.
set -eu

REPO="TokenStick/tokenstick-host"
BIN_DIR=/usr/local/bin
UNIT=/etc/systemd/system/tokenstick.service
DEFAULTS=/etc/default/tokenstick

die() { echo "install.sh: $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
command -v systemctl >/dev/null || die "systemd is required"

if [ "${1:-}" = "--uninstall" ]; then
    systemctl disable --now tokenstick.service 2>/dev/null || true
    rm -f "$UNIT" "$BIN_DIR/tokenstick"
    systemctl daemon-reload
    echo "Removed tokenstick service and binary. Kept $DEFAULTS and /var/lib/tokenstick."
    exit 0
fi

SRC=$(cd "$(dirname "$0")/.." && pwd)

if [ "${1:-}" = "--version" ]; then
    VERSION="${2:-}"
    [ -n "$VERSION" ] || die "--version needs a tag, e.g. v0.1.0"
    case "$(uname -m)" in
        x86_64) TARGET=x86_64-unknown-linux-gnu ;;
        aarch64 | arm64) TARGET=aarch64-unknown-linux-gnu ;;
        *) die "unsupported architecture $(uname -m)" ;;
    esac
    NAME="tokenstick-$VERSION-$TARGET"
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    URL="https://github.com/$REPO/releases/download/$VERSION"
    echo "Downloading $NAME.tar.gz"
    curl -fsSL -o "$TMP/$NAME.tar.gz" "$URL/$NAME.tar.gz"
    curl -fsSL -o "$TMP/$NAME.tar.gz.sha256" "$URL/$NAME.tar.gz.sha256"
    (cd "$TMP" && sha256sum -c "$NAME.tar.gz.sha256")
    tar -xzf "$TMP/$NAME.tar.gz" -C "$TMP"
    SRC="$TMP/$NAME"
fi

[ -x "$SRC/tokenstick" ] || die "no tokenstick binary in $SRC"

install -m 0755 "$SRC/tokenstick" "$BIN_DIR/tokenstick"
install -m 0644 "$SRC/packaging/systemd/tokenstick.service" "$UNIT"
if [ ! -f "$DEFAULTS" ]; then
    install -m 0644 "$SRC/packaging/systemd/tokenstick.default" "$DEFAULTS"
fi

if ! id tokenstick >/dev/null 2>&1; then
    useradd --system --home-dir /var/lib/tokenstick --shell /usr/sbin/nologin tokenstick
fi

systemctl daemon-reload
systemctl enable tokenstick.service >/dev/null
systemctl restart tokenstick.service

echo "Installed $("$BIN_DIR/tokenstick" --version)."
echo "Config: $DEFAULTS   Models: /var/lib/tokenstick/models"
echo "Status: systemctl status tokenstick   Logs: journalctl -u tokenstick -f"
