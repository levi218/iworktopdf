# iwork2pdf (Rust)

Offline converter from Apple iWork documents (`.numbers`, `.key`, `.pages`) to PDF, a port of
the C# `IWorkToPdf`. Pages are drawn with Skia and written by Skia's PDF backend.

## Setup

```bash
cargo build --release
./fetch-pdfium.sh            # PDFium: pasted PDF artwork and the `compare` / `render` commands
```

PDFium is looked up in `IWORK2PDF_PDFIUM`, next to the executable, the working directory, then
the system library path. Font metrics (`apple-metrics.json`) and the optional open lookalike fonts (`open/<family>/`, not
in this repository; missing faces fall back to installed families) are read from
`IWORK2PDF_RESOURCES`, `fonts/` next to the executable, else `iwork2pdf/fonts`.

## Layout

- `iwork2pdf/`: the logic layer, a library with no user interface. `render::render` turns an
  `iwa::IwaDocument::from_bytes` document into PDF bytes in memory; `render::convert` does it file
  to file. The default `pdfium` feature rasterises pasted PDF artwork; build with
  `--no-default-features` where native libraries cannot be loaded (e.g. WASM).
- `iwork2pdf-cli/`: the `iwork2pdf` command line (convert, batch, compare).

## Usage

```bash
iwork2pdf convert doc.numbers -o doc.pdf
iwork2pdf batch <src-dir> <out-dir> [--set list.txt]
iwork2pdf compare <actual-dir> <expected-dir> [--set list.txt] [--cache dir] [--diff-dir dir]
```

`--fonts <dir>` (or `IWORK2PDF_FONTS`) adds licensed font files matched by PostScript name.

## Platform split

Everything platform-specific lives in `iwork2pdf/src/platform/`:

- `macos.rs`: installed Apple fonts are used as the document's exact fonts.
- `windows.rs`: Apple families map to installed lookalikes, positioned with Apple's harvested
  glyph widths; Segoe UI Symbol is the symbol fallback.
- `other.rs`: Linux fallbacks.

## Acceptance metric

`compare` scores each page at 100 dpi in colour:

- **Visual** (pass criterion): both whole pages, text included, compared per RGB channel
  (tolerance 32 after a 2x2 downsample); the score is the fraction of differing pixels.
- **Text** (information only): every character's code point, origin (within 1pt) and fill colour
  (tolerance 32); the score is `(total - 2 * matched) / total` over both pages' characters. Text
  drawn as pixels or paths (e.g. a rasterised pasted PDF) is unmatched here even when it looks right.

A page passes when its visual score is below 1%. `IWORK2PDF_PAGES=1` lists every page's scores.
`--diff-dir` writes colour PNGs per failing page:
expected | actual | faded expected with differences in magenta.

## License

GPL-3.0-or-later (see `LICENSE`).
