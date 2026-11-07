#!/bin/sh
# Downloads a prebuilt PDFium (https://github.com/bblanchon/pdfium-binaries) next to the
# release binary. Usage: ./fetch-pdfium.sh [chromium/<build>]   (default: latest release)
set -eu

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) asset=pdfium-mac-arm64.tgz library=lib/libpdfium.dylib ;;
    Darwin-x86_64) asset=pdfium-mac-x64.tgz library=lib/libpdfium.dylib ;;
    Linux-x86_64) asset=pdfium-linux-x64.tgz library=lib/libpdfium.so ;;
    Linux-aarch64) asset=pdfium-linux-arm64.tgz library=lib/libpdfium.so ;;
    MINGW*-x86_64 | MSYS*-x86_64) asset=pdfium-win-x64.tgz library=bin/pdfium.dll ;;
    *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

if [ $# -gt 0 ]; then
    url="https://github.com/bblanchon/pdfium-binaries/releases/download/$1/$asset"
else
    url="https://github.com/bblanchon/pdfium-binaries/releases/latest/download/$asset"
fi

target_dir="$(cd "$(dirname "$0")" && pwd)/target/release"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

curl -fL "$url" -o "$work_dir/$asset"
tar -xzf "$work_dir/$asset" -C "$work_dir"
mkdir -p "$target_dir"
cp "$work_dir/$library" "$target_dir/"
echo "installed $(basename "$library") in $target_dir"
