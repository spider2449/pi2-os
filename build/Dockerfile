# Reproducible PiOS build environment (used automatically by `cargo xtask`).

# apk.static + Alpine signing keys for every architecture, to install armv7
# helper packages (iptables, e2fsprogs) into the rootfs without emulation.
FROM alpine:3.24 AS apk
RUN apk add --no-cache apk-tools-static alpine-keys

FROM debian:trixie-slim
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential bc bison flex libssl-dev libelf-dev kmod cpio rsync \
        git ca-certificates curl xz-utils python3 python3-pip \
        gcc-arm-linux-gnueabihf binutils-arm-linux-gnueabihf \
        squashfs-tools dosfstools mtools e2fsprogs fdisk genimage openssl \
        qemu-system-arm \
    && rm -rf /var/lib/apt/lists/*

COPY --from=apk /sbin/apk.static /usr/local/bin/apk.static
COPY --from=apk /usr/share/apk/keys /opt/apk-keys

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable \
        --target armv7-unknown-linux-musleabihf

# zig is the C cross compiler/linker for the musl target (needed by mimalloc).
RUN pip3 install --no-cache-dir --break-system-packages ziglang==0.13.0 \
    && cargo install --locked cargo-zigbuild \
    && rm -rf /opt/cargo/registry
