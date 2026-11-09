#!/usr/bin/env bash
# Builds the browser demo's WebAssembly module into iwork2pdf-wasm/web/.
# Needs the Emscripten SDK (`source <emsdk>/emsdk_env.sh`, which sets EMSDK and puts emcc on PATH),
# `rustup target add wasm32-unknown-emscripten`, and iwork2pdf/fonts/open/nimbus-sans/.
set -euo pipefail
cd "$(dirname "$0")/.."
# No debug info in the shipped module (the workspace release profile keeps it for profiling).
CARGO_PROFILE_RELEASE_DEBUG=0 cargo build -p iwork2pdf-wasm --release \
    --target wasm32-unknown-emscripten
cp target/wasm32-unknown-emscripten/release/iwork2pdf-wasm.js \
    target/wasm32-unknown-emscripten/release/iwork2pdf_wasm.wasm iwork2pdf-wasm/web/
echo "Built iwork2pdf-wasm/web/; serve it with: python3 -m http.server -d iwork2pdf-wasm/web"
