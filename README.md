# iwork2pdf (Rust)

Offline converter from Apple iWork documents (`.numbers`, `.key`, `.pages`) to PDF, a port of
the C# `IWorkToPdf`. Pages are drawn with Skia and written by Skia's PDF backend.

## Setup

```bash
cargo build --release
./fetch-pdfium.sh            # PDFium: pasted PDF artwork and the `compare` / `render` commands
```

PDFium is looked up in `IWORK2PDF_PDFIUM`, next to the executable, the working directory, then
the system library path. Font metrics are read from `../IWorkToPdf/fonts` (or
`IWORK2PDF_RESOURCES`, or `fonts/` next to the executable).

## Usage

```bash
iwork2pdf convert doc.numbers -o doc.pdf
iwork2pdf batch <src-dir> <out-dir> [--set list.txt]
iwork2pdf compare <actual-dir> <expected-dir> [--set list.txt] [--cache dir] [--diff-dir dir]
```

`--fonts <dir>` (or `IWORK2PDF_FONTS`) adds licensed font files matched by PostScript name.

## Platform split

Everything platform-specific lives in `src/platform/`:

- `macos.rs`: installed Apple fonts are used as the document's exact fonts.
- `windows.rs`: Apple families map to installed lookalikes, positioned with Apple's harvested
  glyph widths; Segoe UI Symbol is the symbol fallback.
- `other.rs`: Linux fallbacks.

## Acceptance metric

`compare` scores each page twice, both at 100 dpi in colour:

- **Layout**: both pages rendered with their text made invisible, compared per RGB channel
  (tolerance 32 after a 2x2 downsample); the score is the fraction of differing pixels.
- **Text**: every character's code point, origin (within 1pt) and fill colour (tolerance 32);
  the score is `(total - 2 * matched) / total` over both pages' characters.

A page passes when both scores are below 1%. `--diff-dir` writes colour PNGs per failing page:
expected | actual | faded expected with differences in magenta.
