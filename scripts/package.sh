#!/bin/sh
# Builds the release files into dist/. The release workflow runs this too, so
# what you build locally is exactly what gets published.
#
#   alosa-agent-<version>-linux-<arch>.tar.gz   static agent + systemd files + alosa-device
#   alosa-integration-<version>.zip             the Home Assistant integration (manual installs)
#   release.env                                 versions the installer reads
#   SHA256SUMS
#
# Usage: scripts/package.sh [arch...]   arch: x86_64 arm64 armv7 armv6 (default: all)
#
# Needs rustup (for the musl targets) and zip. Everything is pure Rust, so the
# Pi builds need no cross-compiler: Rust's bundled linker does it.
set -eu
cd "$(dirname "$0")/.."

version=$(scripts/version.sh check)
sendspin_version=$(cat crates/alosa-agent/sendspin-cli.version)
archs=${*:-x86_64 arm64 armv7 armv6}

rm -rf dist
mkdir -p dist

for arch in $archs; do
    case $arch in
    x86_64) target=x86_64-unknown-linux-musl ;;
    arm64) target=aarch64-unknown-linux-musl ;;
    armv7) target=armv7-unknown-linux-musleabihf ;;
    armv6) target=arm-unknown-linux-musleabihf ;; # Raspberry Pi Zero / 1
    *)
        echo "unknown arch: $arch (x86_64, arm64, armv7, armv6)" >&2
        exit 1
        ;;
    esac
    echo "building alosa-agent for $arch ($target)"
    rustup target add "$target" >/dev/null
    RUSTFLAGS="-C linker=rust-lld" cargo build --release --locked -p alosa-agent --target "$target"

    name=alosa-agent-$version-linux-$arch
    mkdir -p "dist/$name/systemd"
    cp "target/$target/release/alosa-agent" "dist/$name/"
    cp crates/alosa-agent/systemd/* "dist/$name/systemd/"
    cp scripts/alosa-device.sh "dist/$name/alosa-device"
    (cd dist && tar czf "$name.tar.gz" "$name" && rm -r "$name")
done

echo "packaging the integration"
(cd custom_components/alosa && zip -qr "../../dist/alosa-integration-$version.zip" . -x '*__pycache__*')

printf 'ALOSA_VERSION=%s\nSENDSPIN_CLI_VERSION=%s\n' "$version" "$sendspin_version" >dist/release.env
(cd dist && sha256sum -- *.tar.gz *.zip release.env >SHA256SUMS)

echo "done:"
ls -l dist
