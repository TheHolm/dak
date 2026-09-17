# Prebuilt cross-compilation image used to build dak's FreeBSD .pkg release
# artifact. See NOTES.md section 1 for how this cross-compile setup (sysroot
# file list, clang/lld flags) was originally worked out and verified against
# FreeBSD 14.5-RELEASE.
#
# The FreeBSD sysroot is baked in here, at image-build time, specifically so
# that release builds (.woodpecker/release.yaml) never re-download FreeBSD's
# ~150MB base.txz - it's fetched once, whenever this image is (re)built by
# .woodpecker/build-ci-images.yaml, not on every tagged release.
#
# CAUTION: the exact tar member paths inside base.txz and the FREEBSD_RELEASE
# value below are carried over from the FreeBSD 14.5 verification in
# NOTES.md and have NOT been re-verified against a real FreeBSD 15.x
# base.txz yet (no outbound internet access was available in the sandbox
# this file was authored in). Re-verify on the first real build of this
# image: if the `tar -xJf` step fails to find a member, or the URL 404s,
# check download.freebsd.org's actual directory layout/tar member naming for
# the release in use, adjust FREEBSD_RELEASE / the file list below, then
# update this comment and NOTES.md with what was found (per NOTES.md's own
# "fix it in place" policy).
FROM rust:1.92-trixie

ARG FREEBSD_RELEASE=15.0-RELEASE
ARG FREEBSD_ARCH=amd64
ARG FREEBSD_MAJOR=15

RUN rustup target add x86_64-unknown-freebsd

# clang+lld: cross-linking toolchain (NOTES.md section 1.3).
# zstd: dak-*.pkg files are zstd-compressed tar archives (NOTES.md section 2.1).
# python3: runs scripts/build-freebsd-pkg.py, which assembles the .pkg.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        clang lld zstd python3 \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /opt/freebsd-sysroot
RUN set -eux; \
    curl -fsSL -o /tmp/base.txz \
        "https://download.freebsd.org/releases/${FREEBSD_ARCH}/${FREEBSD_ARCH}/${FREEBSD_RELEASE}/base.txz"; \
    tar -xJf /tmp/base.txz -C /opt/freebsd-sysroot \
        lib/libc.so.7 usr/lib/libc.so usr/lib/libc_nonshared.a \
        lib/libm.so.5 usr/lib/libm.so \
        lib/libutil.so.9 usr/lib/libutil.so \
        lib/librt.so.1 usr/lib/librt.so \
        usr/lib/libexecinfo.so.1 usr/lib/libexecinfo.so \
        lib/libkvm.so.7 usr/lib/libkvm.so \
        usr/lib/libmemstat.so.3 usr/lib/libmemstat.so \
        usr/lib/libprocstat.so.1 usr/lib/libprocstat.so \
        lib/libdevstat.so.7 usr/lib/libdevstat.so \
        lib/libthr.so.3 usr/lib/libthr.so usr/lib/libpthread.so \
        lib/libgcc_s.so.1 usr/lib/libgcc_s.so \
        usr/lib/crt1.o usr/lib/Scrt1.o usr/lib/crti.o usr/lib/crtn.o \
        usr/lib/crtbegin.o usr/lib/crtbeginS.o usr/lib/crtbeginT.o \
        usr/lib/crtend.o usr/lib/crtendS.o; \
    rm -f /tmp/base.txz

# Env vars (rather than a .cargo/config.toml, which NOTES.md section 1.5
# explicitly recommends for CI since the sysroot path here is fixed/known
# ahead of time, unlike a per-developer machine).
ENV CARGO_TARGET_X86_64_UNKNOWN_FREEBSD_LINKER=clang
ENV RUSTFLAGS="-C link-arg=--target=x86_64-unknown-freebsd${FREEBSD_MAJOR} -C link-arg=--sysroot=/opt/freebsd-sysroot -C link-arg=-fuse-ld=lld -C link-arg=-B/opt/freebsd-sysroot/usr/lib"
