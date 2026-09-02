FROM docker.io/library/rust:1.97-trixie AS builder

WORKDIR /work

COPY zccusan/chaos-toolbox/Cargo.toml zccusan/chaos-toolbox/Cargo.lock ./
COPY zccusan/chaos-toolbox/src ./src

RUN cargo build --release

FROM docker.io/library/debian:trixie-slim AS runtime-tools

RUN apt-get update && apt-get install -y --no-install-recommends nftables \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /work/target/release/zccusan-chaos-toolbox \
    /runtime-root/usr/local/bin/zccusan-chaos-toolbox

RUN set -eux; \
    mkdir -p /runtime-root/lib /runtime-root/lib64 /runtime-root/usr; \
    nft_bin="$(command -v nft)"; \
    mkdir -p "/runtime-root$(dirname "${nft_bin}")"; \
    cp "${nft_bin}" "/runtime-root${nft_bin}"; \
    bins="/runtime-root/usr/local/bin/zccusan-chaos-toolbox ${nft_bin}"; \
    for bin in ${bins}; do \
      ldd "${bin}" | awk '/=> \// {print $3} /^\// {print $1}'; \
    done | sort -u | while read -r lib; do \
      mkdir -p "/runtime-root$(dirname "${lib}")"; \
      cp -L "${lib}" "/runtime-root${lib}"; \
    done; \
    for loader in /lib/ld-linux-*.so.* /lib64/ld-linux-*.so.*; do \
      if [ -e "${loader}" ]; then \
        mkdir -p "/runtime-root$(dirname "${loader}")"; \
        cp -L "${loader}" "/runtime-root${loader}"; \
      fi; \
    done

FROM gcr.io/distroless/cc-debian13

COPY --from=runtime-tools /runtime-root/usr/ /usr/
COPY --from=runtime-tools /runtime-root/lib/ /usr/lib/
COPY --from=runtime-tools /runtime-root/lib64/ /usr/lib64/

LABEL org.opencontainers.image.title="zccusan-chaos-toolbox" \
      org.opencontainers.image.description="Cloud-neutral, bounded Linux fault primitives for zccusan reliability tests" \
      org.opencontainers.image.source="https://github.com/robjcaskey/zcutils"

ENV PATH="/usr/local/bin:/usr/sbin:/usr/bin"
USER 0:0
ENTRYPOINT ["/usr/local/bin/zccusan-chaos-toolbox"]
CMD ["agent"]
