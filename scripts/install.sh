#!/bin/sh
# Install the latest Kratos headless engine from GitHub Releases.
#
#   curl -fsSL https://github.com/wasimysaid/Kratos/releases/latest/download/install.sh | sh
#
# Optional, explicit overrides:
#   KRATOS_VERSION=0.2.62              install one tagged release
#   KRATOS_RELEASES_URL=https://...    use a compatible release mirror/test feed
set -eu

REPOSITORY="wasimysaid/Kratos"
LATEST_BASE="https://github.com/$REPOSITORY/releases/latest/download"

fail() {
  echo "kratos install: $*" >&2
  exit 1
}

valid_version() {
  printf '%s\n' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'
}

requested_version="${KRATOS_VERSION:-}"
if [ -n "$requested_version" ]; then
  valid_version "$requested_version" || fail "invalid KRATOS_VERSION '$requested_version'"
  default_base="https://github.com/$REPOSITORY/releases/download/v$requested_version"
else
  default_base="$LATEST_BASE"
fi
base="${KRATOS_RELEASES_URL:-$default_base}"
base="${base%/}"
case "$base" in
  https://*) ;;
  http://127.0.0.1:* | http://localhost:*)
    [ "${KRATOS_INSTALL_ALLOW_INSECURE_LOCALHOST:-}" = 1 ] || fail "release URL must use HTTPS"
    ;;
  *) fail "release URL must use HTTPS" ;;
esac

os="$(uname -s)"
architecture="$(uname -m)"
case "$os" in
  Linux) platform=linux ;;
  Darwin)
    fail "use the macOS desktop release: $LATEST_BASE/kratos-<version>-macos-arm64.dmg"
    ;;
  *) fail "unsupported OS '$os' (the headless installer supports Linux)" ;;
esac
case "$architecture" in
  x86_64 | amd64) architecture=x86_64 ;;
  aarch64 | arm64) architecture=aarch64 ;;
  *) fail "unsupported architecture '$architecture'" ;;
esac

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
curl -fsSL "$base/manifest.json" -o "$tmp/manifest.json"
manifest="$(tr -d '\r\n' < "$tmp/manifest.json")"
version="$(printf '%s' "$manifest" | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
valid_version "$version" || fail "release manifest has an invalid version"
if [ -n "$requested_version" ] && [ "$version" != "$requested_version" ]; then
  fail "release manifest version '$version' does not match requested '$requested_version'"
fi

file="kratos-$version-$platform-$architecture.tar.gz"
checksum="$(printf '%s' "$manifest" | sed -n "s/.*\"$file\"[[:space:]]*:[[:space:]]*{[[:space:]]*\"sha256\"[[:space:]]*:[[:space:]]*\"\([0-9A-Fa-f]*\)\".*/\1/p")"
printf '%s\n' "$checksum" | grep -Eq '^[0-9A-Fa-f]{64}$' \
  || fail "release manifest has no valid SHA-256 for $file"

echo "downloading kratos $version ($platform-$architecture)…"
curl -fSL --progress-bar "$base/$file" -o "$tmp/$file"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$file" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$tmp/$file" | awk '{print $1}')"
elif command -v openssl >/dev/null 2>&1; then
  actual="$(openssl dgst -sha256 "$tmp/$file" | sed 's/^.*= //')"
else
  fail "sha256sum, shasum, or openssl is required to verify the download"
fi
[ "$(printf '%s' "$actual" | tr '[:upper:]' '[:lower:]')" = "$(printf '%s' "$checksum" | tr '[:upper:]' '[:lower:]')" ] \
  || fail "SHA-256 mismatch for $file"

app_root="$HOME/.kratos/app"
destination="$app_root/$version"
mkdir -p "$app_root"
mkdir "$tmp/unpacked"
tar -xzf "$tmp/$file" -C "$tmp/unpacked" --strip-components=1
[ -f "$tmp/unpacked/kratos" ] || fail "$file does not contain a kratos binary"
chmod 755 "$tmp/unpacked/kratos"
rm -rf "$destination"
mv "$tmp/unpacked" "$destination"
ln -sfn "$destination" "$app_root/current"
mkdir -p "$HOME/.local/bin"
ln -sfn "$app_root/current/kratos" "$HOME/.local/bin/kratos"

service=manual
if command -v systemctl >/dev/null 2>&1 && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  mkdir -p "$HOME/.config/systemd/user"
  cat >"$HOME/.config/systemd/user/kratos.service" <<'UNIT'
[Unit]
Description=Kratos native headless engine
After=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
ExecStart=%h/.kratos/app/current/kratos headless
Restart=on-failure
RestartSec=5
EnvironmentFile=-%h/.kratos/env

[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
  systemctl --user enable kratos
  systemctl --user restart kratos
  service=running
  loginctl enable-linger "$USER" 2>/dev/null \
    || sudo -n loginctl enable-linger "$USER" 2>/dev/null \
    || echo "warn: could not enable linger (run: sudo loginctl enable-linger $USER)"
else
  echo "warn: systemd user session unavailable; run: kratos headless"
fi

command -v claude >/dev/null 2>&1 \
  || echo "note: Claude Code CLI not found; install it with: curl -fsSL https://claude.ai/install.sh | bash"

case ":$PATH:" in
  *":$HOME/.local/bin:"*) path_hint="" ;;
  *) path_hint=' (add ~/.local/bin to PATH)' ;;
esac

echo
echo "✓ kratos $version installed$path_hint"
case "$service" in
  running) echo "the engine service is running." ;;
  manual) echo "next: run the local-only engine with 'kratos headless'." ;;
esac
