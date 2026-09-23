#!/bin/sh
set -eu
FFDM_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export RUSTUP_HOME="$FFDM_ROOT/.tools/rustup"
export CARGO_HOME="$FFDM_ROOT/.tools/cargo"
mkdir -p "$RUSTUP_HOME" "$CARGO_HOME"
rustup set auto-self-update disable
rustup toolchain install 1.98.1 --profile minimal --component rustfmt --component clippy
