# Changelog

All notable changes to this project are listed here. Each `## [x.y.z]` section becomes the body
of the GitHub release for tag `vx.y.z` (see `.github/workflows/release.yml`); add the section
before pushing the tag.

## [Unreleased]

## [0.0.1] - 2026-11-13

First release.

### Added

- Conversion of Apple iWork documents (`.pages`, `.numbers`, `.key`, iWork 2013 or later) to
  PDF, drawn with Skia and written by Skia's PDF backend.
- `iwork2pdf` command line: `convert`, `batch`, `compare` (acceptance scoring against reference
  PDFs) and `--version`.
- Pages: sections, page numbering and formats, hyphenation, ligatures, justification and keep
  rules.
- Numbers and Keynote charts, including 3D, mixed, scatter, bubble and radar charts; Keynote
  slide numbers and theme background pictures.
- Platform font handling: Apple fonts on macOS, metric-matched lookalikes on Windows and Linux,
  user-supplied font files via `--fonts`.
- Pasted PDF artwork rasterised with PDFium, or with the pure-Rust hayro when PDFium is off.
- WebAssembly build and a browser demo that converts, previews and downloads documents without
  uploading them, published to GitHub Pages.

### Changed

- Smaller PDFs: JPEGs are passed through, decoded images are shared and fonts are subset.
