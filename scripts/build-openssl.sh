#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
. "$repo/openssl-build.conf"
export LC_ALL=C
base="$repo/target/openssl-$OPENSSL_VERSION"
prefix="$base/install"
archive="$base/openssl-$OPENSSL_VERSION.tar.gz"
mkdir -p "$base"
if [ ! -f "$archive" ]; then
    curl --fail --location --retry 3 --proto '=https' --proto-redir '=https' \
        "https://github.com/openssl/openssl/releases/download/openssl-$OPENSSL_VERSION/openssl-$OPENSSL_VERSION.tar.gz" \
        --output "$archive.part"
    mv "$archive.part" "$archive"
fi
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$archive" | cut -d ' ' -f 1)
else
    actual=$(shasum -a 256 "$archive" | cut -d ' ' -f 1)
fi
[ "$actual" = "$OPENSSL_SHA256" ] || { echo 'OpenSSL checksum mismatch' >&2; exit 1; }
if [ ! -f "$prefix/.xa401-source-sha256" ] || [ "$(cat "$prefix/.xa401-source-sha256")" != "$OPENSSL_SHA256" ]; then
    tar -xzf "$archive" -C "$base"
    cd "$base/openssl-$OPENSSL_VERSION"
    perl ./Configure no-shared no-module no-tests no-comp no-zlib \
        "--prefix=$prefix" --openssldir=/usr/local/ssl --libdir=lib
    make -j "${OPENSSL_BUILD_JOBS:-2}" build_libs
    make install_dev
    printf '%s\n' "$OPENSSL_SHA256" > "$prefix/.xa401-source-sha256"
fi
printf 'OpenSSL %s ready: %s\n' "$OPENSSL_VERSION" "$prefix"
