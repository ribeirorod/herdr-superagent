#!/bin/sh
# Build step for `herdr plugin install ribeirorod/herdr-superagent`.
# 1. Download the prebuilt release binary for this platform into bin/.
# 2. Fall back to `cargo build --release` when no asset is available.
# 3. Link the binary into ~/.local/bin when that directory exists, so
#    `herdr-superagent` is a plain command on PATH (herdr's resume command and
#    workers need that).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/herdr-plugin.toml" | head -n 1)
repo="ribeirorod/herdr-superagent"
mkdir -p "$root/bin"
target="$root/bin/herdr-superagent"

os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
case "$os" in
  linux) os=linux ;;
  darwin) os=macos ;;
  *) echo "install-binary: unsupported OS $os" >&2; exit 1 ;;
esac
case "$arch" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) echo "install-binary: unsupported architecture $arch" >&2; exit 1 ;;
esac

asset="herdr-superagent-$os-$arch"
url="https://github.com/$repo/releases/download/v$version/$asset"

if [ -x "$target" ] && [ "$(if [ "$os" = macos ] && command -v codesign >/dev/null 2>&1; then
  codesign --force --sign - "$target" 2>/dev/null || true
fi

"$target" --version 2>/dev/null | awk '{print $2}')" = "$version" ]; then
  echo "install-binary: bin/herdr-superagent $version already present"
else
  tmp="$target.download"
  if command -v curl >/dev/null 2>&1 && curl -fsSL --retry 2 -o "$tmp" "$url"; then
    chmod +x "$tmp"
    mv "$tmp" "$target"
    echo "install-binary: downloaded $asset $version"
  else
    rm -f "$tmp"
    if command -v cargo >/dev/null 2>&1; then
      echo "install-binary: no release asset at $url; building with cargo (this takes a few minutes)"
      (cd "$root" && cargo build --release --locked)
      # Replace, never overwrite in place: macOS caches the code signature per file and kills a binary copied over an old one.
      rm -f "$target"
      cp "$root/target/release/herdr-superagent" "$target"
    else
      echo "install-binary: no release asset at $url and no cargo on PATH" >&2
      echo "install-binary: install Rust (https://rustup.rs) and run 'herdr plugin update', or download the asset by hand into bin/" >&2
      exit 1
    fi
  fi
fi

"$target" --version

if [ -d "$HOME/.local/bin" ]; then
  ln -sf "$target" "$HOME/.local/bin/herdr-superagent"
  echo "install-binary: linked ~/.local/bin/herdr-superagent"
  case ":${PATH:-}:" in
    *":$HOME/.local/bin:"*) ;;
    *) echo "install-binary: add ~/.local/bin to PATH so herdr can resume the daemon after a restart" ;;
  esac
else
  echo "install-binary: ~/.local/bin does not exist; put $target on PATH yourself"
fi
