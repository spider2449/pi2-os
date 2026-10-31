#!/usr/bin/env bash
# One-time setup of a native PiOS build environment on WSL2 Ubuntu 24.04
# (mirrors build/Dockerfile). Run inside WSL:  bash build/setup-wsl.sh
set -euo pipefail

ZIG_VERSION=0.13.0
ALPINE=v3.24
ALPINE_MIRROR=https://dl-cdn.alpinelinux.org/alpine

echo "==> apt packages (sudo)"
sudo apt-get update
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential bc bison flex libssl-dev libelf-dev kmod cpio rsync \
    git ca-certificates curl xz-utils python3 fakeroot \
    gcc-arm-linux-gnueabihf binutils-arm-linux-gnueabihf \
    squashfs-tools dosfstools mtools e2fsprogs fdisk genimage openssl \
    qemu-system-arm

echo "==> Rust"
if ! command -v rustup >/dev/null; then
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
rustup target add armv7-unknown-linux-musleabihf

echo "==> zig $ZIG_VERSION (C cross compiler for the musl target)"
mkdir -p "$HOME/.local/bin" "$HOME/.local/opt"
if [ ! -x "$HOME/.local/opt/zig-$ZIG_VERSION/zig" ]; then
    curl -fsSL "https://ziglang.org/download/$ZIG_VERSION/zig-linux-x86_64-$ZIG_VERSION.tar.xz" \
        | tar -xJ -C "$HOME/.local/opt"
    mv "$HOME/.local/opt/zig-linux-x86_64-$ZIG_VERSION" "$HOME/.local/opt/zig-$ZIG_VERSION"
fi
ln -sf "$HOME/.local/opt/zig-$ZIG_VERSION/zig" "$HOME/.local/bin/zig"
command -v cargo-zigbuild >/dev/null || cargo install --locked cargo-zigbuild

echo "==> apk.static and Alpine signing keys"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$ALPINE_MIRROR/$ALPINE/main/x86_64/APKINDEX.tar.gz" | tar -xz -C "$tmp" APKINDEX
pkgver() { awk -v p="$1" '$0=="P:"p{f=1} f&&/^V:/{print substr($0,3); exit}' "$tmp/APKINDEX"; }
for pkg in apk-tools-static alpine-keys; do
    v=$(pkgver "$pkg")
    mkdir -p "$tmp/$pkg"
    curl -fsSL "$ALPINE_MIRROR/$ALPINE/main/x86_64/$pkg-$v.apk" | tar -xz -C "$tmp/$pkg" 2>/dev/null || true
done
sudo install -m 0755 "$tmp/apk-tools-static/sbin/apk.static" /usr/local/bin/apk.static
sudo rm -rf /opt/apk-keys
sudo mkdir -p /opt/apk-keys
sudo cp -a "$tmp/alpine-keys/usr/share/apk/keys/." /opt/apk-keys/

echo "==> environment"
profile_line='export PIOS_NATIVE=1 PIOS_WORK_DIR="$HOME/.cache/pios/work" CARGO_TARGET_DIR="$HOME/.cache/pios/target"'
grep -qxF "$profile_line" "$HOME/.profile" || echo "$profile_line" >> "$HOME/.profile"
mkdir -p "$HOME/.cache/pios/work" "$HOME/.cache/pios/target"

echo
echo "Done. Open a new WSL shell, then from the repo (e.g. /mnt/f/coding/otherPrj/pi2-os):"
echo "  cargo xtask all"
