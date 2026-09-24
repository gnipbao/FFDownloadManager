#!/bin/sh
set -eu
PROJECT_ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$PROJECT_ROOT"
if [ "${1:-}" = "--dev" ]; then
  shift
  ./scripts/cargo-local.sh build --locked
  exec ./target/debug/ffdm serve "$@"
else
  ./scripts/cargo-local.sh build --release --locked
  exec ./target/release/ffdm serve "$@"
fi
