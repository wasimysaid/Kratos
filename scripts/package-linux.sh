#!/usr/bin/env bash
# Linux packaging: build the release binary and produce
#   target/package/kratos-<version>-linux-<arch>.tar.gz
# containing the binary, the .desktop entry, and the icon, plus an install.sh
# that drops them into ~/.local (XDG) paths.
#
# Usage: scripts/package-linux.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
ARCH="$(uname -m)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/kratos-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p kratos
  BIN="$ROOT/target/release/kratos"
else
  cargo build -p kratos
  BIN="$ROOT/target/debug/kratos"
fi

GO="${GO:-go}"
GO="$GO" python3 "$ROOT/connectivity/tailcat/licenses/generate.py" --check
GO="$GO" "$ROOT/scripts/build-tailcat.sh" native
TAILCAT_BIN="$ROOT/target/tailcat/kratos-tailcat"

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
install -m 755 "$BIN" "$STAGE/kratos"

install -m 644 "$ROOT/LICENSE" "$STAGE/LICENSE"
install -m 644 "$ROOT/THIRD_PARTY_NOTICES.md" "$STAGE/THIRD_PARTY_NOTICES.md"

install -m 755 "$TAILCAT_BIN" "$STAGE/kratos-tailcat"
install -m 644 "$ROOT/dist/kratos.desktop" "$STAGE/kratos.desktop"
install -m 644 "$ROOT/dist/kratos.png" "$STAGE/kratos.png"
mkdir -p "$STAGE/licenses/fonts"
cp "$ROOT/crates/ui/assets/fonts/licenses/"* "$STAGE/licenses/fonts/"

mkdir -p "$STAGE/licenses/tailcat"
cp -R "$ROOT/connectivity/tailcat/licenses/bundle/." "$STAGE/licenses/tailcat/"

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Kratos into ~/.local (no root needed).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install -Dm755 "$HERE/kratos" "$HOME/.local/bin/kratos"

install -Dm755 "$HERE/kratos-tailcat" "$HOME/.local/bin/kratos-tailcat"
install -Dm644 "$HERE/kratos.desktop" "$HOME/.local/share/applications/kratos.desktop"
install -Dm644 "$HERE/kratos.png" "$HOME/.local/share/icons/hicolor/1024x1024/apps/kratos.png"

install -Dm644 "$HERE/LICENSE" "$HOME/.local/share/doc/kratos/LICENSE"
install -Dm644 "$HERE/THIRD_PARTY_NOTICES.md" "$HOME/.local/share/doc/kratos/THIRD_PARTY_NOTICES.md"
install -d "$HOME/.local/share/doc/kratos/licenses"
cp -R "$HERE/licenses/." "$HOME/.local/share/doc/kratos/licenses/"
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$HOME/.local/share/applications" || true
echo "Installed. Make sure ~/.local/bin is on your PATH."
INSTALL
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"
