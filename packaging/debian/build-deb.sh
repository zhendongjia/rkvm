#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
base_version=$(sed -n '/^version = /{s/^version = "//;s/"$//;p;q;}' \
    "$repo_root/rkvm-server/Cargo.toml")
package_version=${RKVM_PACKAGE_VERSION:-${base_version}+windows.1}
output_dir=${1:-$repo_root/dist}
target_dir=${CARGO_TARGET_DIR:-$repo_root/target}
package_root=$(mktemp -d "${TMPDIR:-/tmp}/rkvm-server-deb.XXXXXX")

cleanup() {
    rm -rf -- "$package_root"
}
trap cleanup EXIT

if [[ ${RKVM_SKIP_BUILD:-0} != 1 ]]; then
    cargo_args=(
        build
        --manifest-path "$repo_root/Cargo.toml"
        --locked
        --release
        -p rkvm-server
        -p rkvm-certificate-gen
    )
    if [[ -d "$repo_root/vendor" ]]; then
        cargo_args+=(
            --offline
            --config 'source.crates-io.replace-with="vendored-sources"'
            --config "source.vendored-sources.directory=\"$repo_root/vendor\""
        )
    fi
    cargo "${cargo_args[@]}"
fi

for binary in rkvm-server rkvm-certificate-gen; do
    if [[ ! -x "$target_dir/release/$binary" ]]; then
        echo "Missing release binary: $target_dir/release/$binary" >&2
        exit 1
    fi
done

install -Dm0755 "$target_dir/release/rkvm-server" \
    "$package_root/usr/bin/rkvm-server"
install -Dm0755 "$target_dir/release/rkvm-certificate-gen" \
    "$package_root/usr/bin/rkvm-certificate-gen"
strip --strip-unneeded "$package_root/usr/bin/rkvm-server" \
    "$package_root/usr/bin/rkvm-certificate-gen"

install -Dm0644 "$script_dir/rkvm-server.service" \
    "$package_root/lib/systemd/system/rkvm-server.service"
install -Dm0644 "$repo_root/example/server.toml" \
    "$package_root/usr/share/doc/rkvm-server/examples/server.toml"
install -Dm0644 "$repo_root/switch-keys.md" \
    "$package_root/usr/share/doc/rkvm-server/switch-keys.md"
install -Dm0644 "$repo_root/LICENSE" \
    "$package_root/usr/share/doc/rkvm-server/copyright"
install -Dm0644 "$script_dir/README.Debian" \
    "$package_root/usr/share/doc/rkvm-server/README.Debian"

install -d -m0755 "$package_root/DEBIAN"
install -m0755 "$script_dir/postinst" "$package_root/DEBIAN/postinst"
install -m0755 "$script_dir/prerm" "$package_root/DEBIAN/prerm"
install -m0755 "$script_dir/postrm" "$package_root/DEBIAN/postrm"

installed_size=$(du -sk "$package_root/usr" "$package_root/lib" | \
    awk '{total += $1} END {print total}')
sed -e "s/@VERSION@/$package_version/g" \
    -e "s/@INSTALLED_SIZE@/$installed_size/g" \
    "$script_dir/control.in" > "$package_root/DEBIAN/control"

mkdir -p "$output_dir"
artifact="$output_dir/rkvm-server_${package_version}_amd64.deb"
dpkg-deb --build --root-owner-group "$package_root" "$artifact"
(cd "$output_dir" && sha256sum "$(basename "$artifact")") > "$artifact.sha256"

dpkg-deb --info "$artifact"
dpkg-deb --contents "$artifact"
printf 'Created %s\n' "$artifact"
