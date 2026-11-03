#!/bin/sh
# Build the lago crate and place the binary at the meson output path.
# Usage: cargo_build.sh <cargo> <Cargo.toml> <target-dir> <output>
set -eu

cargo=$1
manifest=$2
target_dir=$3
output=$4

"$cargo" build --release --offline --locked \
  --manifest-path "$manifest" --target-dir "$target_dir"

cp "$target_dir/release/lago" "$output.tmp"
mv -f "$output.tmp" "$output"
