#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH='' cd -- "$ROOT/../.." && pwd)
GO=${GO:-go}
OUT_DIR=${OUT_DIR:-$REPO/target/tailcat}
MODE=${1:-native}
PKG=./cmd/kratos-tailcat
PIN=fd101889796a
MOBILE_PIN=v0.0.0-20260908204917-8b95e45f8d3e

mkdir -p "$OUT_DIR"
cd "$ROOT"

# Refuse a release build if go.mod no longer contains the reviewed upstream pin.
grep -q "$PIN" go.mod || {
  echo "Tailcat dependency is not pinned to reviewed commit $PIN" >&2
  exit 1
}

grep -q "golang.org/x/mobile $MOBILE_PIN" go.mod || {
  echo "Apple binding tools are not pinned to $MOBILE_PIN" >&2
  exit 1
}
grep -q '^tool golang.org/x/mobile/cmd/gobind$' go.mod || {
  echo "go.mod must retain the pinned gobind tool directive" >&2
  exit 1
}

build_one() {
  os=$1
  arch=$2
  suffix=
  [ "$os" = windows ] && suffix=.exe
  dest="$OUT_DIR/kratos-tailcat-$os-$arch$suffix"
  echo "building $dest" >&2
  CGO_ENABLED=0 GOOS=$os GOARCH=$arch "$GO" build -trimpath -ldflags='-s -w' -o "$dest" "$PKG"
}

build_native() {
  os=$($GO env GOOS)
  arch=$($GO env GOARCH)
  build_one "$os" "$arch"
  cp "$OUT_DIR/kratos-tailcat-$os-$arch" "$OUT_DIR/kratos-tailcat" 2>/dev/null || \
    cp "$OUT_DIR/kratos-tailcat-$os-$arch.exe" "$OUT_DIR/kratos-tailcat.exe"
}

build_cross() {
  build_one linux amd64
  build_one linux arm64
  build_one windows amd64
  build_one windows arm64
  build_one darwin amd64
  build_one darwin arm64
}

build_xcframework() {
  if [ "$(uname -s)" != Darwin ]; then
    echo "XCFramework builds require macOS and Xcode" >&2
    exit 1
  fi
  command -v xcrun >/dev/null 2>&1 || {
    echo "XCFramework builds require Xcode command-line tools" >&2
    exit 1
  }
  GOMOBILE=${GOMOBILE:-gomobile}
  command -v "$GOMOBILE" >/dev/null 2>&1 || {
    echo "gomobile is required (set GOMOBILE to its path)" >&2
    exit 1
  }
  GOMOBILE=$(command -v "$GOMOBILE")
  PATH=$(dirname "$GOMOBILE"):$PATH
  export PATH
  command -v gobind >/dev/null 2>&1 || {
    echo "gobind is required; install golang.org/x/mobile/cmd/gobind@$MOBILE_PIN" >&2
    exit 1
  }
  "$GOMOBILE" bind \
    -target=ios,iossimulator \
    -trimpath \
    -o "$OUT_DIR/KratosTailcat.xcframework" \
    .
}

build_android() {
  GOMOBILE=${GOMOBILE:-gomobile}
  command -v "$GOMOBILE" >/dev/null 2>&1 || {
    echo "gomobile is required (set GOMOBILE to its path)" >&2
    exit 1
  }
  GOMOBILE=$(command -v "$GOMOBILE")
  PATH=$(dirname "$GOMOBILE"):$PATH
  export PATH
  command -v gobind >/dev/null 2>&1 || {
    echo "gobind is required; install golang.org/x/mobile/cmd/gobind@$MOBILE_PIN" >&2
    exit 1
  }
  "$GOMOBILE" bind \
    -target=android \
    -androidapi 24 \
    -trimpath \
    -o "$OUT_DIR/KratosTailcat.aar" \
    .
}

case "$MODE" in
  native) build_native ;;
  cross) build_cross ;;
  xcframework) build_xcframework ;;
  android) build_android ;;
  all)
    build_native
    build_cross
    build_xcframework
    build_android
    ;;
  *)
    echo "usage: $0 {native|cross|xcframework|android|all}" >&2
    exit 2
    ;;
esac
