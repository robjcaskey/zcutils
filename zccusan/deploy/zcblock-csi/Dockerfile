FROM rust:1.89-bookworm AS builder

WORKDIR /work

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    cmake \
    perl \
    pkg-config \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --bin zcblock-csi --bin zcblock-control --bin zcblock-freeze --bin zcrepl --bin zcpit

FROM debian:bookworm-slim AS runtime-tools

RUN apt-get update && apt-get install -y --no-install-recommends \
    e2fsprogs \
    util-linux \
    xfsprogs \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /work/target/release/zcblock-csi /runtime-root/usr/local/bin/zcblock-csi
COPY --from=builder /work/target/release/zcblock-control /runtime-root/usr/local/bin/zcblock-control
COPY --from=builder /work/target/release/zcblock-freeze /runtime-root/usr/local/bin/zcblock-freeze
COPY --from=builder /work/target/release/zcrepl /runtime-root/usr/local/bin/zcrepl
COPY --from=builder /work/target/release/zcpit /runtime-root/usr/local/bin/zcpit
COPY allowed-raw-partitions.txt /runtime-root/etc/zcblock-csi/allowed-raw-partitions.txt

RUN set -eux; \
    bins="/runtime-root/usr/local/bin/zcblock-csi /runtime-root/usr/local/bin/zcblock-control /runtime-root/usr/local/bin/zcblock-freeze /runtime-root/usr/local/bin/zcrepl /runtime-root/usr/local/bin/zcpit /usr/bin/mount /usr/bin/umount /usr/bin/findmnt /usr/sbin/blkid /usr/sbin/losetup /usr/sbin/mkfs.ext4 /usr/sbin/mkfs.xfs /usr/sbin/fsfreeze"; \
    for bin in /usr/bin/mount /usr/bin/umount /usr/bin/findmnt /usr/sbin/blkid /usr/sbin/losetup /usr/sbin/mkfs.ext4 /usr/sbin/mkfs.xfs /usr/sbin/fsfreeze; do \
      mkdir -p "/runtime-root$(dirname "$bin")"; \
      cp "$bin" "/runtime-root$bin"; \
    done; \
    for bin in $bins; do \
      ldd "$bin" | awk '/=> \// {print $3} /^\// {print $1}'; \
    done | sort -u | while read -r lib; do \
      mkdir -p "/runtime-root$(dirname "$lib")"; \
      cp -L "$lib" "/runtime-root$lib"; \
    done; \
    if [ -e /etc/mke2fs.conf ]; then \
      mkdir -p /runtime-root/etc; \
      cp /etc/mke2fs.conf /runtime-root/etc/mke2fs.conf; \
    fi

FROM gcr.io/distroless/cc-debian12

COPY --from=runtime-tools /runtime-root/ /

ENV PATH="/usr/local/bin:/usr/sbin:/usr/bin"
USER 0:0
ENTRYPOINT ["/usr/local/bin/zcblock-csi"]
