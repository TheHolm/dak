# Prebuilt image used to build dak's Ubuntu 26.04 LTS .deb package.
#
# Same rebuild-on-change / no-registry-push story as deb-trixie.Dockerfile -
# see that file and .woodpecker/build-ci-images.yaml's header comment.
#
# Ubuntu ships no official Rust base image (unlike Debian's `rust:*-trixie`),
# so the toolchain is installed via rustup instead. Pinned to the same Rust
# release (1.92) as deb-trixie.Dockerfile / docker/Dockerfile so both .debs
# are built with an identical compiler version - only the target distro's
# glibc/runtime libraries differ between the two packages.
FROM ubuntu:26.04

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        curl ca-certificates build-essential libudev-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain 1.92.0

RUN cargo install cargo-deb --locked
