# syntax=docker/dockerfile:1.7
# Production image. The build stage runs on the build machine's own architecture and
# cross-compiles a static binary for the target (linux/arm64 in production) with
# cargo-zigbuild, so an ARM64 image builds at native speed instead of under QEMU.

FROM --platform=$BUILDPLATFORM rust:1.94-bookworm AS build
ARG TARGETARCH
ARG BUILDARCH
# Zig (the cross linker) comes from its PyPI wheel, verified against a pinned checksum.
ARG ZIG_VERSION=0.13.0
ARG ZIG_SHA256_amd64=3ce0c9f16547e5d61b32e0d226926e9a2552ef4b91fccf7ab5ea1a623a77824b
ARG ZIG_SHA256_arm64=5714a4d46f9246ba6180a0930e23df2bc8bc26e62b5fe0dc798b4afca1903308
# Optional extra CA for builds behind a TLS-inspecting proxy; used only in this stage.
RUN --mount=type=secret,id=extra_ca,required=false \
    set -e; \
    if [ -s /run/secrets/extra_ca ]; then cp /run/secrets/extra_ca /usr/local/share/ca-certificates/build-extra-ca.crt && update-ca-certificates >/dev/null; fi; \
    case "$BUILDARCH" in amd64) tag=x86_64; sum=$ZIG_SHA256_amd64 ;; arm64) tag=aarch64; sum=$ZIG_SHA256_arm64 ;; *) echo "unsupported build arch $BUILDARCH"; exit 1 ;; esac; \
    url=$(curl -fsSL "https://pypi.org/pypi/ziglang/$ZIG_VERSION/json" | python3 -c "import json,sys; print(next(u['url'] for u in json.load(sys.stdin)['urls'] if 'manylinux' in u['filename'] and '_'+sys.argv[1] in u['filename']))" "$tag"); \
    curl -fsSL "$url" -o /tmp/zig.whl; \
    echo "$sum  /tmp/zig.whl" | sha256sum -c -; \
    unzip -q /tmp/zig.whl -d /opt/zig && rm /tmp/zig.whl; \
    printf '#!/bin/sh\nexec /opt/zig/ziglang/zig "$@"\n' > /usr/local/bin/zig && chmod +x /usr/local/bin/zig /opt/zig/ziglang/zig; \
    zig version; \
    cargo install --locked cargo-zigbuild@0.19.8; \
    rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -e; \
    case "$TARGETARCH" in arm64) T=aarch64-unknown-linux-musl ;; amd64) T=x86_64-unknown-linux-musl ;; *) echo "unsupported $TARGETARCH"; exit 1 ;; esac; \
    cargo zigbuild --release --locked --bin callora --target "$T"; \
    install -D "target/$T/release/callora" /out/callora; \
    mkdir -p /out/data/voice-library

FROM gcr.io/distroless/static-debian12:nonroot
LABEL org.opencontainers.image.source="https://github.com/ben12211/callora"
LABEL org.opencontainers.image.description="Callora V2 phone agent"
COPY --from=build /out/callora /usr/local/bin/callora
# Empty and owned by the runtime user, so a fresh voice-library volume is writable.
COPY --from=build --chown=65532:65532 /out/data /data
COPY businesses /app/businesses
WORKDIR /app
ENV BUSINESS_CONFIG_DIR=/app/businesses \
    AUDIO_LIBRARY_DIR=/data/voice-library \
    HOST=0.0.0.0 \
    PORT=3000
USER nonroot
EXPOSE 3000
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=5 CMD ["/usr/local/bin/callora", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/callora"]
CMD ["serve"]
