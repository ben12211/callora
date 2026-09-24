# syntax=docker/dockerfile:1.7
# Development toolchain: the official Rust image plus clippy and rustfmt. Used by ./dev.
FROM rust:1.94-bookworm

# An optional extra CA (for networks behind a TLS-inspecting proxy) is used only while
# this step downloads, and is not left in the image.
RUN --mount=type=secret,id=extra_ca,required=false \
    set -e; \
    if [ -s /run/secrets/extra_ca ]; then \
      cp /run/secrets/extra_ca /usr/local/share/ca-certificates/build-extra-ca.crt && update-ca-certificates >/dev/null; \
    fi; \
    rustup component add clippy rustfmt; \
    if [ -f /usr/local/share/ca-certificates/build-extra-ca.crt ]; then \
      rm /usr/local/share/ca-certificates/build-extra-ca.crt && update-ca-certificates --fresh >/dev/null; \
    fi
