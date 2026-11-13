#!/bin/sh
# Downloads Nimbus Sans (URW base35, AGPL with font exception; not in this repository) into
# iwork2pdf/fonts/open/nimbus-sans/: the WASM demo embeds it and release archives ship it.
set -eu
dir="$(cd "$(dirname "$0")" && pwd)/iwork2pdf/fonts/open/nimbus-sans"
mkdir -p "$dir"
base=https://raw.githubusercontent.com/ArtifexSoftware/urw-base35-fonts/master
curl -fsSL -o "$dir/COPYING" "$base/COPYING"
for face in NimbusSans-Regular NimbusSans-Bold NimbusSans-Italic NimbusSans-BoldItalic \
            NimbusSansNarrow-Regular NimbusSansNarrow-Bold NimbusSansNarrow-Oblique \
            NimbusSansNarrow-BoldOblique; do
    curl -fsSL -o "$dir/$face.otf" "$base/fonts/$face.otf"
done
echo "installed Nimbus Sans in $dir"
