#!/usr/bin/env sh
set -eu

echo "Building Radelta native codec (release)..."
cargo build --release

echo "Running release tests..."
cargo test --release

case "$(uname -s)" in
    Linux)
        built="target/release/libradelta_native.so"
        public="target/release/libradelta.so"
        ;;
    Darwin)
        built="target/release/libradelta_native.dylib"
        public="target/release/libradelta.dylib"
        ;;
    *)
        echo "Unsupported platform for native-library staging: $(uname -s)" >&2
        exit 1
        ;;
esac

if [ ! -f "$built" ]; then
    echo "Native Cargo artifact not found: $built" >&2
    exit 1
fi
cp -f "$built" "$public"

echo
echo "CLI: target/release/radelta"
echo "Shared library: $public"
echo "C header: include/radelta.h"
