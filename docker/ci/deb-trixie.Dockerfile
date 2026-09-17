# Prebuilt image used to build dak's Debian trixie (Debian 13) .deb package.
#
# Rebuilt only by .woodpecker/build-ci-images.yaml, when this file changes -
# release builds (.woodpecker/release.yaml) just consume the resulting local
# image tag (dak-ci-deb-trixie:latest, pull: false) directly from the single
# agent host's Docker image store. See that workflow's header comment for
# why this means no registry push is needed and no apt-get/cargo-install
# downloads happen during an actual release build.
#
# Base matches docker/Dockerfile's existing dev-container Rust pin, so the
# toolchain that produces release .debs matches the one already used for
# local development/testing.
FROM rust:1.92-trixie

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        libudev-dev \
    && rm -rf /var/lib/apt/lists/*

# cargo-deb reads Cargo.toml's [package] fields (name/version/description/
# license/repository - see Cargo.toml) to generate the package manifest; no
# [package.metadata.deb] section exists yet, so cargo-deb falls back to its
# defaults (runtime Depends auto-derived by inspecting the built binary's
# shared library needs via ldd, docs staged from Cargo.toml's readme/license
# fields). Revisit this if the package ever needs a maintainer script,
# systemd unit, or explicit Depends/Conflicts.
RUN cargo install cargo-deb --locked
