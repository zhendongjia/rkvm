#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
version=$(sed -n '/^version = /{s/^version = "//;s/"$//;p;q;}' \
    "$repo_root/rkvm-client/Cargo.toml")
product_version="${version}.0"
output_dir=${1:-$repo_root/dist}
certificate=${RKVM_CERTIFICATE:-$repo_root/target/deploy/certificate.pem}
default_server=${RKVM_DEFAULT_SERVER:-10.24.32.7:5258}
client="$repo_root/target/x86_64-pc-windows-gnu/release/rkvm-client.exe"
driver_root="$repo_root/rkvm-windows-driver"
devcon="$driver_root/packages/Microsoft.Windows.WDK.x64.10.0.28000.2526/c/tools/10.0.28000.0/x64/devcon.exe"

cargo build --manifest-path "$repo_root/Cargo.toml" --locked --release \
    --target x86_64-pc-windows-gnu -p rkvm-client

required=(
    "$client"
    "$certificate"
    "$driver_root/x64/Release/rkvmvhid.cer"
    "$driver_root/x64/Release/rkvmvhid/rkvmvhid.inf"
    "$driver_root/x64/Release/rkvmvhid/rkvmvhid.cat"
    "$driver_root/x64/Release/rkvmvhid/rkvmvhid.sys"
    "$devcon"
)
for path in "${required[@]}"; do
    if [[ ! -f "$path" ]]; then
        echo "Missing installer input: $path" >&2
        exit 1
    fi
done

mkdir -p "$output_dir"
output_dir=$(cd "$output_dir" && pwd)
artifact="$output_dir/rkvm-windows-${version}-setup.exe"

makensis \
    -DVERSION="$version" \
    -DPRODUCT_VERSION="$product_version" \
    -DDEFAULT_SERVER="$default_server" \
    -DCLIENT_EXE="$(cygpath -w "$client")" \
    -DCERTIFICATE_FILE="$(cygpath -w "$certificate")" \
    -DDRIVER_ROOT="$(cygpath -w "$driver_root")" \
    -DDEVCON_EXE="$(cygpath -w "$devcon")" \
    -DINSTALL_CLIENT_SCRIPT="$(cygpath -w "$script_dir/install-client.ps1")" \
    -DREADME_FILE="$(cygpath -w "$script_dir/README-Windows.txt")" \
    -DLICENSE_FILE="$(cygpath -w "$repo_root/LICENSE")" \
    -DOUTPUT_EXE="$(cygpath -w "$artifact")" \
    "$(cygpath -w "$script_dir/rkvm.nsi")"

checksum=
for attempt in {1..20}; do
    if checksum=$(cd "$output_dir" && \
            sha256sum "$(basename "$artifact")" 2>/dev/null); then
        break
    fi
    if [[ $attempt == 20 ]]; then
        echo "Could not checksum installer after waiting for its file lock." >&2
        exit 1
    fi
    sleep 0.25
done
printf '%s\n' "$checksum" > "$artifact.sha256"
printf 'Created %s\n' "$artifact"
