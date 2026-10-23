#!/bin/sh
# Build the release tarball for this machine, or run the tests the same way.
#   scripts/release.sh        build dist/torrential-<target>.tar.gz and its .sha256
#   scripts/release.sh test   run cargo test --workspace with the same toolchain
set -eu
cd "$(dirname "$0")/.."
mode=${1:-build}
case "$mode" in
  build | test) ;;
  *)
    echo "usage: scripts/release.sh [build | test]" >&2
    exit 2
    ;;
esac

case "$(uname -s) $(uname -m)" in
  "Darwin arm64") target=aarch64-apple-darwin ;;
  "Linux x86_64") target=x86_64-unknown-linux-musl ;;
  "Linux aarch64") target=aarch64-unknown-linux-musl ;;
  *)
    echo "release.sh builds on macOS arm64 or Linux arm64 or x86_64, not $(uname -s) $(uname -m)" >&2
    exit 1
    ;;
esac

case "$target" in
  *-apple-darwin)
    export MACOSX_DEPLOYMENT_TARGET=14.0
    if [ "$mode" = test ]; then
      exec nix --extra-experimental-features 'nix-command flakes' develop -c cargo test --workspace --locked
    fi
    nix --extra-experimental-features 'nix-command flakes' develop -c cargo build --release --locked -p torrential
    bin=target/release/torrential
    if otool -L "$bin" | tail -n +2 | grep -Ev '^[[:space:]]+(/usr/lib/|/System/)'; then
      echo "$bin links a library outside /usr/lib and /System" >&2
      exit 1
    fi
    minos=$(vtool -show-build "$bin" | awk '$1 == "minos" { print $2 }')
    if [ "$minos" != 14.0 ]; then
      echo "$bin needs macOS $minos, not 14.0" >&2
      exit 1
    fi
    ;;
  *-linux-musl)
    if [ "$mode" = test ]; then
      cargo="cargo test --workspace --locked"
    else
      cargo="cargo build --release --locked -p torrential"
    fi
    docker run --rm --init -v "$PWD:/src" -w /src -e OWNER="$(id -u):$(id -g)" -e CARGO="$cargo" \
      -e RUSTUP_HOME=/src/target/linux/rustup -e CARGO_HOME=/src/target/linux/cargo alpine:3.24 sh -euc '
      apk add --no-cache build-base cmake samurai boost-dev openssl-dev openssl-libs-static linux-headers curl file >/dev/null
      [ -x "$CARGO_HOME/bin/cargo" ] || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path >/dev/null 2>&1
      export PATH="$CARGO_HOME/bin:$PATH"
      status=0
      BOOST_ROOT=/usr OPENSSL_INCLUDE_DIR=/usr/include OPENSSL_LIB_DIR=/usr/lib $CARGO --target-dir target/linux || status=$?
      chown -R "$OWNER" target/linux
      exit $status
    '
    [ "$mode" = test ] && exit 0
    bin=target/linux/release/torrential
    if ! file "$bin" | grep -q 'static'; then
      file "$bin" >&2
      echo "$bin is not statically linked" >&2
      exit 1
    fi
    ;;
esac

mkdir -p dist
asset=torrential-$target.tar.gz
stage=$(mktemp -d)
cp "$bin" LICENSE "$stage/"
tar -czf "dist/$asset" -C "$stage" torrential LICENSE
rm -rf "$stage"

cd dist
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$asset" > "$asset.sha256"
else
  shasum -a 256 "$asset" > "$asset.sha256"
fi
cat "$asset.sha256"
