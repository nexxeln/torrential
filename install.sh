#!/bin/sh
set -eu

repo=nexxeln/torrential
dir=${TORRENTIAL_INSTALL_DIR:-$HOME/.local/bin}
version=${TORRENTIAL_VERSION:-latest}

case "$(uname -s) $(uname -m)" in
  "Darwin arm64") target=aarch64-apple-darwin ;;
  "Linux x86_64") target=x86_64-unknown-linux-musl ;;
  "Linux aarch64" | "Linux arm64") target=aarch64-unknown-linux-musl ;;
  *)
    echo "torrential has no prebuilt binary for $(uname -s) $(uname -m)." >&2
    echo "Build it from source: https://github.com/$repo#build-from-source" >&2
    exit 1
    ;;
esac

if [ "$version" = latest ]; then
  url=https://github.com/$repo/releases/latest/download
else
  url=https://github.com/$repo/releases/download/$version
fi

asset=torrential-$target.tar.gz
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $asset"
for file in "$asset" "$asset.sha256"; do
  if ! curl -fsSL "$url/$file" -o "$tmp/$file"; then
    echo "Couldn't download $url/$file" >&2
    exit 1
  fi
done

cd "$tmp"
if command -v sha256sum >/dev/null 2>&1; then
  check="sha256sum -c"
else
  check="shasum -a 256 -c"
fi
if ! $check "$asset.sha256" >/dev/null 2>&1; then
  echo "$asset doesn't match its published checksum, so nothing was installed." >&2
  exit 1
fi
tar -xzf "$asset"

mkdir -p "$dir"
cp torrential "$dir/.torrential.new"
chmod 755 "$dir/.torrential.new"
mv -f "$dir/.torrential.new" "$dir/torrential"
echo "Installed $("$dir/torrential" --version) to $dir/torrential"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Add $dir to your PATH to run it: export PATH=\"$dir:\$PATH\"" ;;
esac
