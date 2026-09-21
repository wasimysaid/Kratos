#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/fixture/payload/kratos-1.2.3-linux-aarch64"
printf '#!/bin/sh\necho kratos fixture\n' >"$TMP/fixture/payload/kratos-1.2.3-linux-aarch64/kratos"
chmod 755 "$TMP/fixture/payload/kratos-1.2.3-linux-aarch64/kratos"
tar -czf "$TMP/fixture/asset.tar.gz" -C "$TMP/fixture/payload" kratos-1.2.3-linux-aarch64
sha="$(sha256sum "$TMP/fixture/asset.tar.gz" | awk '{print $1}')"
printf '{"version":"1.2.3","files":{"kratos-1.2.3-linux-aarch64.tar.gz":{"sha256":"%s"}}}\n' "$sha" >"$TMP/fixture/manifest.json"

cat >"$TMP/bin/uname" <<'MOCK'
#!/bin/sh
case "${1:-}" in
  -s) echo Linux ;;
  -m) echo arm64 ;;
  *) echo Linux ;;
esac
MOCK
cat >"$TMP/bin/curl" <<'MOCK'
#!/bin/sh
set -eu
url=""
out=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
printf '%s\n' "$url" >>"$CURL_LOG"
case "$url" in
  */manifest.json) cp "$FIXTURE_DIR/manifest.json" "$out" ;;
  */kratos-1.2.3-linux-aarch64.tar.gz) cp "$FIXTURE_DIR/asset.tar.gz" "$out" ;;
  *) echo "unexpected URL: $url" >&2; exit 22 ;;
esac
MOCK
chmod 755 "$TMP/bin/uname" "$TMP/bin/curl"

HOME="$TMP/home" PATH="$TMP/bin:$PATH" FIXTURE_DIR="$TMP/fixture" CURL_LOG="$TMP/curl.log" \
  KRATOS_RELEASES_URL="http://127.0.0.1:9876/releases" \
  KRATOS_INSTALL_ALLOW_INSECURE_LOCALHOST=1 XDG_RUNTIME_DIR= \
  sh "$ROOT/scripts/install.sh" >/dev/null

test -x "$TMP/home/.kratos/app/1.2.3/kratos"
test "$(readlink "$TMP/home/.kratos/app/current")" = "$TMP/home/.kratos/app/1.2.3"
grep -qx 'http://127.0.0.1:9876/releases/manifest.json' "$TMP/curl.log"
grep -qx 'http://127.0.0.1:9876/releases/kratos-1.2.3-linux-aarch64.tar.gz' "$TMP/curl.log"

# A corrupt replacement must fail checksum validation before changing current.
printf 'corrupt' >"$TMP/fixture/asset.tar.gz"
if HOME="$TMP/home" PATH="$TMP/bin:$PATH" FIXTURE_DIR="$TMP/fixture" CURL_LOG="$TMP/curl.log" \
  KRATOS_RELEASES_URL="http://127.0.0.1:9876/releases" \
  KRATOS_INSTALL_ALLOW_INSECURE_LOCALHOST=1 XDG_RUNTIME_DIR= \
  sh "$ROOT/scripts/install.sh" >"$TMP/corrupt.out" 2>&1; then
  echo "corrupt release unexpectedly installed" >&2
  exit 1
fi
grep -q 'SHA-256 mismatch' "$TMP/corrupt.out"
test -x "$TMP/home/.kratos/app/1.2.3/kratos"

# Reject malformed versions before any network request.
: >"$TMP/curl.log"
if HOME="$TMP/other-home" PATH="$TMP/bin:$PATH" FIXTURE_DIR="$TMP/fixture" CURL_LOG="$TMP/curl.log" \
  KRATOS_VERSION='../bad' XDG_RUNTIME_DIR= sh "$ROOT/scripts/install.sh" >"$TMP/version.out" 2>&1; then
  echo "malformed version unexpectedly accepted" >&2
  exit 1
fi
grep -q 'invalid KRATOS_VERSION' "$TMP/version.out"
test ! -s "$TMP/curl.log"

echo "installer tests passed"
