#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/../../.." && pwd)
OUTPUT="$ROOT/target/tailcat/KratosTailcat.aar"

GOMOBILE=${GOMOBILE:-gomobile} "$ROOT/scripts/build-tailcat.sh" android

test -f "$OUTPUT" || {
  echo "KratosTailcat.aar was not produced" >&2
  exit 1
}
