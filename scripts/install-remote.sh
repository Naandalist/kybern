#!/bin/sh
# Install the daemon and CLI from one pinned release, verifying its published SHA-256
# before extraction. The desktop embeds this script; it never executes a downloaded
# installer. --service installs a persistent user service; --service-only only starts it.
# Linux needs a systemd user manager and lingering. macOS needs a logged-in GUI user
# for its LaunchAgent. Windows uses the release's PowerShell installer separately.
set -eu
VERSION="${KYBERN_VERSION:-}"
SERVICE=0
INSTALL=1
PORT=4173
BIND=127.0.0.1
BIN="${KYBERN_INSTALL_DIR:-$HOME/.cargo/bin}"
DATA="${KYBERN_DATA_DIR:-$HOME/.kybern}"
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift ;;
    --version=*) VERSION="${1#--version=}" ;;
    --service) SERVICE=1 ;;
    --service-only) SERVICE=1; INSTALL=0 ;;
    --port) PORT="$2"; shift ;;
    --port=*) PORT="${1#--port=}" ;;
    --bind) BIND="$2"; shift ;;
    --bind=*) BIND="${1#--bind=}" ;;
    -h|--help) echo "Install Kybern: --version X.Y.Z [--service] [--port 4173]. Start an installed daemon: --service-only."; exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done
fail() { echo "$*" >&2; exit 1; }
case "$PORT" in ''|*[!0-9]*) fail 'Use a port from 1 to 65535.';; esac
[ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || fail 'Use a port from 1 to 65535.'
[ "$BIND" = '127.0.0.1' ] || fail 'Remote setup binds only to 127.0.0.1; configure exposure in Kybern.'
OS=$(uname -s)
case "$OS" in
  Linux) PLATFORM=unknown-linux-musl ;;
  Darwin) PLATFORM=apple-darwin ;;
  *) fail 'Automatic SSH installation supports Linux and macOS. On Windows, install kybern-installer.ps1 from the release page, configure a persistent service, then add its pairing invitation.' ;;
esac
if [ "$SERVICE" = 1 ]; then
  # These values enter systemd and launchd files, which have different escaping rules.
  # Refuse ambiguous paths rather than interpreting shell or service directives.
  case "$BIN:$DATA:$HOME" in *[!a-zA-Z0-9_./:-]*) fail 'Service setup needs paths containing only letters, digits, dots, slashes, dashes and underscores. Choose a compatible data folder.';; esac
  case "$BIN:$DATA" in /*:/*) ;; *) fail 'Use absolute installation and data folders.';; esac
fi
if [ "$INSTALL" = 1 ]; then
  # Latest is resolved once, then both archive and checksum use that exact release.
  command -v curl >/dev/null 2>&1 || fail 'Install curl on this machine and try again.'
  if [ -z "$VERSION" ] || [ "$VERSION" = latest ]; then
    VERSION=$(curl --proto '=https' --tlsv1.2 --connect-timeout 15 --max-time 60 -fsSL https://github.com/Tresnanda/kybern/releases/latest/download/latest.json | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([0-9][0-9.]*\)".*/\1/p' | head -1)
  fi
  VERSION=${VERSION#v}
  # A dev release (X.Y.Z-dev.N) installs too: a desktop on the Dev track sets up its machines at its own version.
  printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-dev\.[0-9]+)?$' || fail 'Choose a published Kybern release version, for example 0.19.0 or 0.20.0-dev.1.'
  case "$(uname -m)" in x86_64|amd64) ARCH=x86_64;; arm64|aarch64) ARCH=aarch64;; *) fail 'No published build for this processor. Install Kybern from source.';; esac
  ARCHIVE="kybern-$ARCH-$PLATFORM.tar.xz"
  BASE="https://github.com/Tresnanda/kybern/releases/download/v$VERSION"
  STAGE=$(mktemp -d "${TMPDIR:-/tmp}/kybern-install.XXXXXX")
  trap 'rm -rf "$STAGE"' EXIT
  trap 'exit 130' HUP INT TERM
  curl --proto '=https' --tlsv1.2 --connect-timeout 15 --max-time 300 -fsSL "$BASE/$ARCHIVE" -o "$STAGE/release.tar.xz"
  EXPECTED=$(curl --proto '=https' --tlsv1.2 --connect-timeout 15 --max-time 60 -fsSL "$BASE/$ARCHIVE.sha256" | awk 'NR == 1 {print tolower($1)}')
  if command -v sha256sum >/dev/null 2>&1; then ACTUAL=$(sha256sum "$STAGE/release.tar.xz" | awk '{print $1}')
  elif command -v shasum >/dev/null 2>&1; then ACTUAL=$(shasum -a 256 "$STAGE/release.tar.xz" | awk '{print $1}')
  else fail 'Install sha256sum or shasum on this machine and try again.'; fi
  [ "${#EXPECTED}" = 64 ] && [ "$EXPECTED" = "$ACTUAL" ] || fail 'The downloaded archive does not match its published checksum. Try again later.'
  tar -xf "$STAGE/release.tar.xz" -C "$STAGE"
  mkdir -p "$BIN"
  for NAME in kybernd kybern; do
    SOURCE=$(find "$STAGE" -type f -name "$NAME" | head -1)
    [ -n "$SOURCE" ] || fail "The release is missing $NAME. Try another release."
    cp "$SOURCE" "$BIN/.$NAME.new"
    chmod 755 "$BIN/.$NAME.new"
    mv -f "$BIN/.$NAME.new" "$BIN/$NAME"
  done
fi
if [ "$SERVICE" = 1 ]; then
  # Already installed binaries may live elsewhere; keep that exact executable.
  if [ "$INSTALL" = 0 ]; then BIN=$(dirname "$(command -v kybernd)"); fi
  case "$BIN" in *[!a-zA-Z0-9_./-]*) fail 'Move the daemon to a service-compatible path and try again.';; esac
  [ -x "$BIN/kybernd" ] || fail 'Install kybernd first, then try again.'
  mkdir -p "$DATA"
  if [ "$OS" = Linux ]; then
    command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1 || fail 'A systemd user manager is required. Enable it for this SSH user, then retry.'
    # Do not ask for a password or silently claim persistence when logout would stop it.
    command -v loginctl >/dev/null 2>&1 || fail 'Install loginctl and enable lingering for this user, then retry.'
    if ! loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null | grep -qx yes; then
      loginctl enable-linger "$(id -un)" </dev/null >/dev/null 2>&1 || fail 'Ask an administrator to run loginctl enable-linger for this SSH user, then retry.'
    fi
    loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null | grep -qx yes || fail 'Lingering is not enabled. Ask an administrator to enable it for this user, then retry.'
    UNIT="$HOME/.config/systemd/user/kybernd.service"
    mkdir -p "$(dirname "$UNIT")"
    if [ -f "$UNIT" ]; then
      grep -Fq -- "--data-dir $DATA" "$UNIT" || fail 'An existing kybernd service uses another configuration. Update that service on this machine, then retry.'
      # Units written before OOMPolicy=continue stop the daemon and every agent when one agent runs out of memory.
      if ! grep -Eq '^OOMPolicy=' "$UNIT" && ! grep -Eqs '^OOMPolicy=' "$UNIT.d"/*.conf; then
        mkdir -p "$UNIT.d"
        printf '[Service]\nOOMPolicy=continue\n' > "$UNIT.d/oom.conf"
      fi
    else
      (set -C; cat > "$UNIT" <<UNIT
[Unit]
Description=Kybern agent daemon
[Service]
ExecStart=$BIN/kybernd --bind 127.0.0.1 --port $PORT --data-dir $DATA
WorkingDirectory=$HOME
Environment="PATH=$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin"
Restart=on-failure
RestartSec=5
KillSignal=SIGINT
TimeoutStopSec=30
OOMPolicy=continue
[Install]
WantedBy=default.target
UNIT
      )
    fi
    systemctl --user daemon-reload
    systemctl --user enable --now kybernd
    systemctl --user is-active --quiet kybernd || fail 'The kybernd service did not start. Check journalctl --user -u kybernd, then retry.'
  else
    DOMAIN="gui/$(id -u)"
    launchctl print "$DOMAIN" >/dev/null 2>&1 || fail 'Sign in to the Mac desktop once to start its user LaunchAgent, then retry. Headless system services need administrator setup.'
    PLIST="$HOME/Library/LaunchAgents/dev.kybern.daemon.plist"
    mkdir -p "$(dirname "$PLIST")"
    if [ -f "$PLIST" ]; then
      grep -Fq "<string>$DATA</string>" "$PLIST" || fail 'An existing Kybern LaunchAgent uses another data folder. Update it on the Mac, then retry.'
    else
      (set -C; cat > "$PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>dev.kybern.daemon</string>
<key>ProgramArguments</key><array><string>$BIN/kybernd</string><string>--bind</string><string>127.0.0.1</string><string>--port</string><string>$PORT</string><string>--data-dir</string><string>$DATA</string></array>
<key>EnvironmentVariables</key><dict><key>PATH</key><string>$HOME/.cargo/bin:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string></dict>
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
<key>StandardOutPath</key><string>$DATA/daemon.log</string>
<key>StandardErrorPath</key><string>$DATA/daemon.log</string>
</dict></plist>
PLIST
      )
    fi
    if ! launchctl print "$DOMAIN/dev.kybern.daemon" >/dev/null 2>&1; then launchctl bootstrap "$DOMAIN" "$PLIST"; fi
    launchctl kickstart "$DOMAIN/dev.kybern.daemon"
  fi
fi
