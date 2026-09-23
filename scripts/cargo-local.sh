#!/bin/sh
set -eu
FFDM_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export RUSTUP_HOME="$FFDM_ROOT/.tools/rustup"
export CARGO_HOME="$FFDM_ROOT/.tools/cargo"
# Reuse the already installed project-local stable toolchain only when its
# version matches the pin. Never update the user's global default toolchain.
FFDM_VERSION=1.98.1
case "$(rustup run stable rustc --version 2>/dev/null || true)" in
  "rustc $FFDM_VERSION "*) exec cargo +stable "$@" ;;
  *) exec cargo +"$FFDM_VERSION" "$@" ;;
esac
