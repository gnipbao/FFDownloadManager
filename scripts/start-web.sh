#!/bin/sh
set -eu
PROJECT_ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$PROJECT_ROOT"
./scripts/cargo-local.sh build --release --locked
exec ./target/release/ffdm serve "$@"
