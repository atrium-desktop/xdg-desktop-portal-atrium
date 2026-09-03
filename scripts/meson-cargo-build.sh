#!/bin/sh
set -eu

mode=$1
source_root=$2
build_root=$3
cargo=$4
shift 4

target_dir="$build_root/cargo-target"

case "$mode" in
  portal)
    test "$#" -eq 2
    (
      cd "$source_root"
      "$cargo" build \
        --manifest-path "$source_root/Cargo.toml" \
        --target-dir "$target_dir" \
        --locked --release \
        -p xdg-desktop-portal-atrium \
        -p atrium-portal-prompter
    )
    cp "$target_dir/release/xdg-desktop-portal-atrium" "$1"
    cp "$target_dir/release/atrium-portal-prompter" "$2"
    ;;
  pam)
    test "$#" -eq 1
    (
      cd "$source_root"
      "$cargo" build \
        --manifest-path "$source_root/Cargo.toml" \
        --target-dir "$target_dir" \
        --locked --release \
        -p atrium-pam
    )
    cp "$target_dir/release/libpam_atrium.so" "$1"
    ;;
  *)
    printf 'unknown artifact mode: %s\n' "$mode" >&2
    exit 2
    ;;
esac
