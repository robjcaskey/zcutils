# Keep the builder distro aligned with runtime-tools: the libfabric headers,
# link-time library, and shipped provider must come from the same Debian ABI.
FROM docker.io/library/rust:1.97-trixie AS builder

WORKDIR /work

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    cmake \
    libfabric-dev \
    perl \
    pkg-config \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY src ./src

RUN RUST_MIN_STACK=33554432 cargo build --release --jobs 16 \
    --bin zcutils \
    --bin zcblock-csi \
    --bin zcblock-node-setup \
    --bin zcblock-control \
    --bin zcblock-freeze \
    --bin zccusan-telemetry-server \
    --bin zccusan-operator \
    --bin zcrepl \
    --bin zcpit \
    --bin zctier \
    --bin zcnblk-fan \
    --bin zcnblk-shm-target \
    --bin zcnblk-wal-failover \
    --bin zcnblk-wal-leaf \
    --bin zcnblk-order-smoke

FROM docker.io/library/debian:trixie-slim AS runtime-tools

RUN apt-get update && apt-get install -y --no-install-recommends \
    e2fsprogs \
    ibverbs-providers \
    libfabric-bin \
    libfabric1 \
    util-linux \
    xfsprogs \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /work/target/release/zcblock-csi /runtime-root/usr/local/bin/zcblock-csi
COPY --from=builder /work/target/release/zcblock-node-setup /runtime-root/usr/local/bin/zcblock-node-setup
COPY --from=builder /work/target/release/zcutils /runtime-root/usr/local/bin/zcutils
COPY --from=builder /work/target/release/zcblock-control /runtime-root/usr/local/bin/zcblock-control
COPY --from=builder /work/target/release/zcblock-freeze /runtime-root/usr/local/bin/zcblock-freeze
COPY --from=builder /work/target/release/zccusan-telemetry-server /runtime-root/usr/local/bin/zccusan-telemetry-server
COPY --from=builder /work/target/release/zccusan-operator /runtime-root/usr/local/bin/zccusan-operator
COPY --from=builder /work/target/release/zcrepl /runtime-root/usr/local/bin/zcrepl
COPY --from=builder /work/target/release/zcpit /runtime-root/usr/local/bin/zcpit
COPY --from=builder /work/target/release/zctier /runtime-root/usr/local/bin/zctier
COPY --from=builder /work/target/release/zcnblk-fan /runtime-root/usr/local/bin/zcnblk-fan
COPY --from=builder /work/target/release/zcnblk-shm-target /runtime-root/usr/local/bin/zcnblk-shm-target
COPY --from=builder /work/target/release/zcnblk-wal-failover /runtime-root/usr/local/bin/zcnblk-wal-failover
COPY --from=builder /work/target/release/zcnblk-wal-leaf /runtime-root/usr/local/bin/zcnblk-wal-leaf
COPY --from=builder /work/target/release/zcnblk-order-smoke /runtime-root/usr/local/bin/zcnblk-order-smoke
COPY zccusan/deploy/zcblock-csi/allowed-raw-partitions.empty.txt /runtime-root/etc/zcblock-csi/allowed-raw-partitions.txt

RUN set -eux; \
    mkdir -p /runtime-root/lib /runtime-root/lib64 /runtime-root/usr /runtime-root/etc; \
    bins="/runtime-root/usr/local/bin/zcutils /runtime-root/usr/local/bin/zcblock-csi /runtime-root/usr/local/bin/zcblock-node-setup /runtime-root/usr/local/bin/zcblock-control /runtime-root/usr/local/bin/zcblock-freeze /runtime-root/usr/local/bin/zccusan-telemetry-server /runtime-root/usr/local/bin/zccusan-operator /runtime-root/usr/local/bin/zcrepl /runtime-root/usr/local/bin/zcpit /runtime-root/usr/local/bin/zctier /runtime-root/usr/local/bin/zcnblk-fan /runtime-root/usr/local/bin/zcnblk-shm-target /runtime-root/usr/local/bin/zcnblk-wal-failover /runtime-root/usr/local/bin/zcnblk-wal-leaf /runtime-root/usr/local/bin/zcnblk-order-smoke /usr/bin/fi_info /usr/bin/mount /usr/bin/umount /usr/bin/findmnt /usr/sbin/blkid /usr/sbin/losetup /usr/sbin/mkfs.ext4 /usr/sbin/mkfs.xfs /usr/sbin/fsfreeze"; \
    for bin in /usr/bin/fi_info /usr/bin/mount /usr/bin/umount /usr/bin/findmnt /usr/sbin/blkid /usr/sbin/losetup /usr/sbin/mkfs.ext4 /usr/sbin/mkfs.xfs /usr/sbin/fsfreeze; do \
      mkdir -p "/runtime-root$(dirname "$bin")"; \
      cp "$bin" "/runtime-root$bin"; \
    done; \
    for provider_dir in /usr/lib/*-linux-gnu/libibverbs; do \
      if [ -d "$provider_dir" ]; then \
        target_dir="/runtime-root$provider_dir"; \
        mkdir -p "$target_dir"; \
        for provider in "$provider_dir"/*; do \
          [ -e "$provider" ] || continue; \
          cp -L "$provider" "$target_dir/$(basename "$provider")"; \
          bins="$bins $provider"; \
        done; \
      fi; \
    done; \
    if [ -d /etc/libibverbs.d ]; then \
      mkdir -p /runtime-root/etc; \
      cp -a /etc/libibverbs.d /runtime-root/etc/libibverbs.d; \
    fi; \
    for bin in $bins; do \
      ldd "$bin" | awk '/=> \// {print $3} /^\// {print $1}'; \
    done | sort -u | while read -r lib; do \
      mkdir -p "/runtime-root$(dirname "$lib")"; \
      if [ -e "$lib" ]; then \
        cp -L "$lib" "/runtime-root$lib"; \
      fi; \
    done; \
    for loader in /lib/ld-linux-*.so.* /lib64/ld-linux-*.so.*; do \
      if [ -e "$loader" ]; then \
        mkdir -p "/runtime-root$(dirname "$loader")"; \
        cp -L "$loader" "/runtime-root$loader"; \
      fi; \
    done; \
    if [ -e /etc/mke2fs.conf ]; then \
      mkdir -p /runtime-root/etc; \
      cp /etc/mke2fs.conf /runtime-root/etc/mke2fs.conf; \
    fi

# CI replaces this default empty directory with the audited, architecture-local
# exact-ABI bundle. Keeping the copy unconditional makes local image builds
# deterministic and keeps all module delivery in this one OCI image.
ARG KMOD_BUNDLE_ROOT=zccusan/deploy/zcblock-csi/kmod-bundle
COPY ${KMOD_BUNDLE_ROOT}/ /runtime-root/opt/zcutils/kmods/

FROM gcr.io/distroless/cc-debian13

# Distroless uses /lib -> /usr/lib (and, where present, /lib64 ->
# /usr/lib64).  Copying a staged /lib directory over that symlink fails on
# arm64, so merge the staged libraries into the symlink targets explicitly.
COPY --from=runtime-tools /runtime-root/usr/ /usr/
COPY --from=runtime-tools /runtime-root/lib/ /usr/lib/
COPY --from=runtime-tools /runtime-root/lib64/ /usr/lib64/
COPY --from=runtime-tools /runtime-root/etc/ /etc/
COPY --from=runtime-tools /runtime-root/opt/ /opt/

ENV PATH="/usr/local/bin:/usr/sbin:/usr/bin"
USER 0:0
ENTRYPOINT ["/usr/local/bin/zcblock-csi"]
