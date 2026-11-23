#!/bin/sh
set -eu
exec cargo run --locked --release --manifest-path "$(dirname "$0")/../Cargo.toml" -p macos-client -- "$@"
