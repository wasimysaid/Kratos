#!/usr/bin/env bash
# Native end-to-end sync smoke. Each test builds and launches the project-local
# Tailcat adapter and real Rust peer; no Wrangler, hosted backend, or dev bearer
# is involved.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"

run_test() {
  local package="$1" target="$2"
  echo "==> cargo test -p ${package} --test ${target} -- --test-threads=1"
  cargo test -p "$package" --test "$target" -- --test-threads=1
}

run_test kratos-engine real_tailcat
run_test kratos-engine peer_preservation
run_test kratos-sync peer_clients
