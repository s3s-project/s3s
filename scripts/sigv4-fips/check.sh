#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors
set -euo pipefail
cd "$(dirname "$0")/../.."
temp=$(mktemp -d)
trap 'rm -r "$temp"' EXIT

# Use the headers and the native library selected by Cargo, including its symbol prefix.
cargo build --locked -p s3s-sigv4 --lib --no-default-features --features fips --message-format=json > "$temp/build.json"
out=$(jq -r 'select(.reason == "build-script-executed" and (.package_id | contains("#aws-lc-fips-sys@"))) | .out_dir' "$temp/build.json")
rlib=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "s3s_sigv4") | .filenames[] | select(endswith(".rlib"))' "$temp/build.json")
native=$(jq -r 'select(.reason == "build-script-executed" and (.package_id | contains("#aws-lc-fips-sys@"))) | .linked_paths[] | select(startswith("native=")) | ltrimstr("native=")' "$temp/build.json")
test -f "$out/include/openssl/service_indicator.h" && test -f "$rlib" && test -d "$native" || {
    echo "Could not locate the Rust library and its AWS-LC FIPS build artifacts" >&2
    exit 1
}
case $(uname -s) in
    Darwin) entry=-Wl,-e,_fips_probe_main ;;
    Linux)  entry=-Wl,--wrap=main ;;
    *) echo "Run the native service indicator check on Linux or macOS" >&2; exit 1 ;;
esac

cc -std=c11 -Wall -Werror -I"$out/include" -c scripts/sigv4-fips/observer.c -o "$temp/observer.o"
rustc --edition=2024 -F unsafe-code scripts/sigv4-fips/probe.rs --extern "s3s_sigv4=$rlib" \
    -L "dependency=$(dirname "$rlib")/deps" -L "native=$native" \
    -C "link-arg=$temp/observer.o" -C "link-arg=$entry" -C "link-arg=-Wl,-rpath,$native" -o "$temp/probe"
for operation in control sha256 single-fragment fragments stream hmac; do
    "$temp/probe" "$operation"
done
