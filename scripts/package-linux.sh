#!/usr/bin/env bash
set -Eeuo pipefail
export LC_ALL=C

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
target=${1:-}
if [[ $target == --help ]]; then
    printf 'Usage: bash scripts/package-linux.sh [x86_64-unknown-linux-musl|aarch64-unknown-linux-musl]\n'
    exit 0
fi
[[ $(uname -s) == Linux ]] || { printf 'Build inside Linux, WSL, Docker or GitHub Actions.\n' >&2; exit 1; }
case $(uname -m) in
    x86_64) native_target=x86_64-unknown-linux-musl; arch=amd64 ;;
    aarch64|arm64) native_target=aarch64-unknown-linux-musl; arch=arm64 ;;
    *) printf 'Unsupported CPU architecture.\n' >&2; exit 1 ;;
esac
target=${target:-$native_target}
[[ $target == "$native_target" ]] || { printf 'Use a build runner with the same CPU architecture as the target.\n' >&2; exit 1; }
cd -- "$project_dir"
rustup target add "$target"
if command -v musl-gcc >/dev/null; then
    export "CC_${target//-/_}=musl-gcc"
    linker_variable="CARGO_TARGET_${target^^}_LINKER"
    export "${linker_variable//-/_}=musl-gcc"
fi
cargo build --locked --release --target "$target" -j 1
binary="$project_dir/target/$target/release/skyblock-price-service"
if readelf -l "$binary" | grep -q 'INTERP'; then
    printf 'Refusing to package a binary that requires a dynamic ELF interpreter.\n' >&2
    exit 1
fi
mkdir -p "$project_dir/dist"
stage=$(mktemp -d "$project_dir/target/package.XXXXXXXX")
cleanup() {
    if [[ $stage == "$project_dir/target/package."* ]]; then
        rm -rf -- "$stage"
    fi
}
trap cleanup EXIT
bundle="$stage/skyblock-price-service"
mkdir -p "$bundle/deploy" "$bundle/docs"
install -m 755 "$binary" "$bundle/skyblock-price-service"
install -m 755 deploy/install.sh "$bundle/deploy/install.sh"
install -m 644 config.example.toml THIRD_PARTY_NOTICES.txt "$bundle/"
install -m 644 docs/deployment.md docs/api.md "$bundle/docs/"
printf 'target=%s\nbuilt_at=%s\n' "$target" "$(date -u +%FT%TZ)" > "$bundle/BUILD_INFO.txt"
rustc --version >> "$bundle/BUILD_INFO.txt"
(
    cd -- "$bundle"
    find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS
)
archive="$project_dir/dist/skyblock-price-service-linux-$arch.tar.gz"
tar -czf "$archive" -C "$stage" skyblock-price-service
(
    cd -- "$project_dir/dist"
    sha256sum "$(basename -- "$archive")" > "$(basename -- "$archive").sha256"
)
printf '\nLinux package: %s\n' "$archive"
