//! Page-by-page acceptance comparison of a converted PDF against Apple's export, in colour.
//!
//! - **Visual** (the pass criterion): both whole pages, text included, are rendered by PDFium at
//!   100 dpi on white, box-downsampled 2x2 (50 dpi effective, which forgives sub-pixel
//!   anti-aliasing of hairlines and glyph edges), and a pixel differs when any of its red, green or
//!   blue channels differs by more than 32/255. The score is the fraction of the page area that
//!   differs.
//! - **Text** (information only): every character PDFium extracts from Apple's page should appear
//!   on ours with the same Unicode value, its origin within 1pt and its fill colour within 32/255
//!   per channel, with no extra characters. The score is the unmatched fraction of all characters;
//!   text drawn as pixels or paths (a rasterised pasted PDF) counts as unmatched here although it
//!   may look right.
//!
//! A page passes when its visual score is below 1%. The metric is fixed: do not tune it to make files
//! pass.

use crate::drawables::raster;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use pdfium_render::prelude::*;
use rayon::prelude::*;
use skia_safe::{AlphaType, EncodedImageFormat};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Rasterisation resolution.
pub const DPI: f32 = 100.0;
/// Per-channel tolerance of the visual comparison (after the 2x2 downsample).
pub const CHANNEL_TOLERANCE: u8 = 32;
/// How far a character's origin may move, in points.
pub const POSITION_TOLERANCE: f32 = 1.0;
/// A page passes when its visual score is below this fraction.
pub const THRESHOLD: f64 = 0.01;
/// Pages larger than this many pixels are skipped unless IWORK2PDF_FULL=1.
const MAX_PAGE_PIXELS: f64 = 60e6;
/// Bumped when the cached data's meaning changes.
const CACHE_FORMAT: u32 = 3;

/// Comparison failures that stop a file from being compared.
#[derive(Debug, thiserror::Error)]
pub enum CompareError {
    /// PDFium could not be loaded.
    #[error("libpdfium not found (set IWORK2PDF_PDFIUM or put it next to the executable)")]
    NoPdfium,
    /// A PDF could not be read or rendered.
    #[error("{0}")]
    Pdf(String),
}

impl From<PdfiumError> for CompareError {
    fn from(error: PdfiumError) -> CompareError {
        CompareError::Pdf(error.to_string())
    }
}

/// One character as PDFium extracts it: Unicode value, origin (points, y up) and fill colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextChar {
    /// The Unicode scalar value.
    pub code: u32,
    /// Origin x.
    pub origin_x: f32,
    /// Origin y.
    pub origin_y: f32,
    /// Fill colour (red, green, blue).
    pub color: [u8; 3],
}

/// One rendered page: its text-free pixels (RGB) and its characters.
#[derive(Debug, Clone, PartialEq)]
pub struct PageCapture {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// RGB pixels of the page with its text removed.
    pub pixels: Vec<u8>,
    /// The page's characters (whitespace left out).
    pub chars: Vec<TextChar>,
}

/// The scores of one page.
#[derive(Debug, Clone, PartialEq)]
pub struct PageScore {
    /// 1-based page number.
    pub page: usize,
    /// Fraction of the page area (text included) whose pixels differ.
    pub visual: f64,
    /// Fraction of characters unmatched (missing, extra, moved or recoloured).
    pub text: f64,
    /// Why the page could not be compared, if it could not.
    pub error: Option<String>,
}

impl PageScore {
    fn failed(page: usize, error: String) -> PageScore {
        PageScore {
            page,
            visual: 1.0,
            text: 1.0,
            error: Some(error),
        }
    }

    /// The visual score is below the threshold (the text score is information only).
    pub fn passed(&self) -> bool {
        self.error.is_none() && self.visual < THRESHOLD
    }
}

/// The comparison of one file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileScore {
    /// Pages in Apple's PDF.
    pub expected_pages: usize,
    /// Per-page scores (empty when the page counts differ).
    pub pages: Vec<PageScore>,
    /// Why the file could not be compared page by page.
    pub error: Option<String>,
}

impl FileScore {
    /// Every page passed.
    pub fn passed(&self) -> bool {
        self.error.is_none() && self.pages.iter().all(PageScore::passed)
    }

    /// The page with the worst visual score.
    pub fn worst(&self) -> Option<&PageScore> {
        self.pages
            .iter()
            .max_by(|first, second| first.visual.total_cmp(&second.visual))
    }
}

/// Where expected pages are cached between runs (Apple's PDFs do not change).
#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    /// A cache in `dir` (created on first write).
    pub fn new(dir: &Path) -> Cache {
        Cache {
            dir: dir.to_path_buf(),
        }
    }

    /// The cache file of a page: keyed by the PDF's path, size and modification time.
    fn file(&self, pdf: &Path, page: usize) -> Option<PathBuf> {
        let metadata = std::fs::metadata(pdf).ok()?;
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let identity = format!(
            "{}|{}|{modified}|{DPI}|{CACHE_FORMAT}|{page}",
            pdf.canonicalize().ok()?.display(),
            metadata.len()
        );
        Some(
            self.dir
                .join(format!("{:016x}.page", fnv1a(identity.as_bytes()))),
        )
    }

    fn load(&self, pdf: &Path, page: usize) -> Option<PageCapture> {
        let mut bytes = Vec::new();
        ZlibDecoder::new(std::fs::File::open(self.file(pdf, page)?).ok()?)
            .read_to_end(&mut bytes)
            .ok()?;
        decode_capture(&bytes)
    }

    fn store(&self, pdf: &Path, page: usize, capture: &PageCapture) {
        let Some(file) = self.file(pdf, page) else {
            return;
        };
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&self.dir)?;
            // Write to a temporary file and rename, so parallel workers never read a partial entry.
            let temporary = file.with_extension(format!("{}.tmp", std::process::id()));
            let mut encoder = ZlibEncoder::new(
                std::fs::File::create(&temporary)?,
                flate2::Compression::fast(),
            );
            encoder.write_all(&encode_capture(capture))?;
            encoder.finish()?;
            std::fs::rename(&temporary, &file)
        };
        if let Err(error) = write() {
            log::warn!("compare cache: {}: {error}", file.display());
        }
    }
}

/// 64-bit FNV-1a hash (stable across runs, unlike the standard library's hasher).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Serialises a capture: width, height, character count, characters, pixels.
fn encode_capture(capture: &PageCapture) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(12 + capture.chars.len() * 15 + capture.pixels.len());
    for value in [capture.width, capture.height, capture.chars.len()] {
        bytes.extend((value as u32).to_le_bytes());
    }
    for character in &capture.chars {
        bytes.extend(character.code.to_le_bytes());
        bytes.extend(character.origin_x.to_le_bytes());
        bytes.extend(character.origin_y.to_le_bytes());
        bytes.extend(character.color);
    }
    bytes.extend(&capture.pixels);
    bytes
}

fn decode_capture(bytes: &[u8]) -> Option<PageCapture> {
    use crate::bytes::le_u32;
    let field = |index: usize| le_u32(bytes, index * 4).map(|value| value as usize);
    let (width, height, count) = (field(0)?, field(1)?, field(2)?);
    let float = |offset: usize| le_u32(bytes, offset).map(f32::from_bits);
    let chars = (0..count)
        .map(|index| {
            let at = 12 + index * 15;
            Some(TextChar {
                code: le_u32(bytes, at)?,
                origin_x: float(at + 4)?,
                origin_y: float(at + 8)?,
                color: bytes.get(at + 12..at + 15)?.try_into().ok()?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let pixels = bytes.get(12 + count * 15..)?;
    (pixels.len() == width * height * 3).then(|| PageCapture {
        width,
        height,
        pixels: pixels.to_vec(),
        chars,
    })
}

/// Renders the whole page and extracts its characters.
fn capture(page: &PdfPage) -> Result<PageCapture, CompareError> {
    let chars = page_chars(page);
    let (width, height, rgba) =
        crate::pdfium::render(page, DPI, Some(PdfColor::WHITE)).map_err(CompareError::Pdf)?;
    let pixels = rgba
        .chunks_exact(4)
        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect();
    Ok(PageCapture {
        width,
        height,
        pixels,
        chars,
    })
}

/// The page's visible characters (whitespace and control characters left out).
fn page_chars(page: &PdfPage) -> Vec<TextChar> {
    let Ok(text) = page.text() else {
        return Vec::new();
    };
    let chars = text.chars();
    chars
        .iter()
        .filter_map(|character| {
            let code = character.unicode_value();
            let visible = char::from_u32(code)
                .is_some_and(|value| !value.is_whitespace() && !value.is_control());
            if !visible {
                return None;
            }
            let (origin_x, origin_y) = character.origin().ok()?;
            let color = character.fill_color().ok()?;
            Some(TextChar {
                code,
                origin_x: origin_x.value,
                origin_y: origin_y.value,
                color: [color.red(), color.green(), color.blue()],
            })
        })
        .collect()
}

/// The fraction of differing pixels after a 2x2 box downsample, and the differing pixels' mask.
fn page_difference(actual: &PageCapture, expected: &PageCapture) -> (f64, Vec<bool>, usize, usize) {
    let (width, height) = (
        actual.width.min(expected.width),
        actual.height.min(expected.height),
    );
    let (half_width, half_height) = (width / 2, height / 2);
    // Each output pixel is the rounded mean of a 2x2 block.
    let downsample = |pixels: &[u8], stride: usize| -> Vec<u8> {
        (0..half_width * half_height)
            .into_par_iter()
            .flat_map_iter(|index| {
                let (column, row) = (index % half_width * 2, index / half_width * 2);
                let at = move |dx: usize, dy: usize, channel: usize| {
                    u32::from(pixels[((row + dy) * stride + column + dx) * 3 + channel])
                };
                (0..3).map(move |channel| {
                    ((at(0, 0, channel)
                        + at(1, 0, channel)
                        + at(0, 1, channel)
                        + at(1, 1, channel)
                        + 2)
                        / 4) as u8
                })
            })
            .collect()
    };
    let (ours, theirs) = (
        downsample(&actual.pixels, actual.width),
        downsample(&expected.pixels, expected.width),
    );
    let mask: Vec<bool> = ours
        .par_chunks_exact(3)
        .zip(theirs.par_chunks_exact(3))
        .map(|(ours, theirs)| {
            ours.iter()
                .zip(theirs)
                .any(|(ours, theirs)| ours.abs_diff(*theirs) > CHANNEL_TOLERANCE)
        })
        .collect();
    let differing = mask.iter().filter(|&&differs| differs).count();
    let fraction = if mask.is_empty() {
        0.0
    } else {
        differing as f64 / mask.len() as f64
    };
    (fraction, mask, half_width, half_height)
}

/// The fraction of characters without a counterpart: same code, origin within the position
/// tolerance, colour within the channel tolerance. Each of ours matches at most once.
fn text_difference(actual: &[TextChar], expected: &[TextChar]) -> f64 {
    let total = actual.len() + expected.len();
    if total == 0 {
        return 0.0;
    }
    let mut ours: HashMap<u32, Vec<(TextChar, bool)>> = HashMap::new();
    for character in actual {
        ours.entry(character.code)
            .or_default()
            .push((*character, false));
    }
    let mut matched = 0;
    for theirs in expected {
        let Some(candidates) = ours.get_mut(&theirs.code) else {
            continue;
        };
        let distance = |candidate: &TextChar| {
            (candidate.origin_x - theirs.origin_x).hypot(candidate.origin_y - theirs.origin_y)
        };
        let same_color = |candidate: &TextChar| {
            candidate
                .color
                .iter()
                .zip(theirs.color)
                .all(|(ours, expected)| ours.abs_diff(expected) <= CHANNEL_TOLERANCE)
        };
        let nearest = candidates
            .iter_mut()
            .filter(|(candidate, used)| {
                !used && distance(candidate) <= POSITION_TOLERANCE && same_color(candidate)
            })
            .min_by(|first, second| distance(&first.0).total_cmp(&distance(&second.0)));
        if let Some(candidate) = nearest {
            candidate.1 = true;
            matched += 1;
        }
    }
    (total - 2 * matched) as f64 / total as f64
}

/// Writes expected | actual | difference side by side, in colour: the difference panel shows
/// Apple's page faded toward white with differing pixels in magenta.
fn write_overlay(
    path: &Path,
    expected: &PageCapture,
    actual: &PageCapture,
    (mask, mask_width, mask_height): (&[bool], usize, usize),
) {
    let (width, height) = (
        expected.width.min(actual.width),
        expected.height.min(actual.height),
    );
    let panel = width + 8;
    let mut rgba = vec![255u8; panel * 3 * height * 4];
    let put = |rgba: &mut [u8], column: usize, row: usize, color: [u8; 3]| {
        let at = (row * panel * 3 + column) * 4;
        rgba[at..at + 3].copy_from_slice(&color);
    };
    for row in 0..height {
        for column in 0..width {
            let pixel = |capture: &PageCapture| {
                let at = (row * capture.width + column) * 3;
                [
                    capture.pixels[at],
                    capture.pixels[at + 1],
                    capture.pixels[at + 2],
                ]
            };
            put(&mut rgba, column, row, pixel(expected));
            put(&mut rgba, panel + column, row, pixel(actual));
            let differs = (column / 2 < mask_width && row / 2 < mask_height)
                && mask[(row / 2) * mask_width + column / 2];
            let faded = pixel(expected).map(|channel| 128 + channel / 2);
            put(
                &mut rgba,
                2 * panel + column,
                row,
                if differs { [230, 0, 200] } else { faded },
            );
        }
    }
    let Some(image) = raster(panel * 3, height, &rgba, AlphaType::Unpremul) else {
        return;
    };
    let encoded = image.encode(None, EncodedImageFormat::PNG, None);
    let written = encoded
        .ok_or_else(|| std::io::Error::other("PNG encoding failed"))
        .and_then(|data| {
            std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
            std::fs::write(path, data.as_bytes())
        });
    if let Err(error) = written {
        log::warn!("diff image {}: {error}", path.display());
    }
}

/// Options of a comparison run.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Where to write colour diff images of failing pages.
    pub diff_dir: Option<PathBuf>,
    /// Where to cache Apple's rendered pages.
    pub cache: Option<Cache>,
    /// Also compare pages over 60 million pixels.
    pub full: bool,
}

/// Compares a converted PDF with Apple's.
///
/// # Arguments
///
/// * `actual` - Our PDF
/// * `expected` - Apple's PDF
/// * `options` - Diff image directory, cache, huge pages
///
/// # Returns
///
/// Per-page visual and text scores.
///
/// # Errors
///
/// Returns `CompareError` when PDFium is missing or a PDF cannot be opened.
pub fn compare(
    actual: &Path,
    expected: &Path,
    options: &Options,
) -> Result<FileScore, CompareError> {
    let pdfium = crate::pdfium::get().ok_or(CompareError::NoPdfium)?;
    let ours = pdfium.load_pdf_from_file(actual, None)?;
    let theirs = pdfium.load_pdf_from_file(expected, None)?;
    let (our_count, their_count) = (
        usize::from(ours.pages().len()),
        usize::from(theirs.pages().len()),
    );
    if our_count != their_count {
        let error = Some(format!("page count {our_count} vs expected {their_count}"));
        return Ok(FileScore {
            expected_pages: their_count,
            pages: Vec::new(),
            error,
        });
    }
    let name = expected
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut pages = Vec::with_capacity(their_count);
    for index in 0..their_count {
        let page_index = index as PdfPageIndex;
        let our_page = ours.pages().get(page_index)?;
        let pixels = f64::from(our_page.width().value * DPI / 72.0)
            * f64::from(our_page.height().value * DPI / 72.0);
        if pixels > MAX_PAGE_PIXELS && !options.full {
            pages.push(PageScore::failed(
                index + 1,
                "skipped: page too large for the quick loop".into(),
            ));
            continue;
        }
        let cached = options
            .cache
            .as_ref()
            .and_then(|cache| cache.load(expected, index));
        let expected_capture = match cached {
            Some(capture) => capture,
            None => {
                let capture = capture(&theirs.pages().get(page_index)?)?;
                if let Some(cache) = &options.cache {
                    cache.store(expected, index, &capture);
                }
                capture
            }
        };
        let actual_capture = capture(&our_page)?;
        if actual_capture.width.abs_diff(expected_capture.width) > 1
            || actual_capture.height.abs_diff(expected_capture.height) > 1
        {
            let error = format!(
                "page {} size {}x{}px vs expected {}x{}px",
                index + 1,
                actual_capture.width,
                actual_capture.height,
                expected_capture.width,
                expected_capture.height
            );
            pages.push(PageScore::failed(index + 1, error));
            continue;
        }
        let (visual, mask, mask_width, mask_height) =
            page_difference(&actual_capture, &expected_capture);
        let text = text_difference(&actual_capture.chars, &expected_capture.chars);
        if let (Some(dir), true) = (&options.diff_dir, visual >= THRESHOLD) {
            write_overlay(
                &dir.join(format!("{name}-p{}.png", index + 1)),
                &expected_capture,
                &actual_capture,
                (&mask, mask_width, mask_height),
            );
        }
        pages.push(PageScore {
            page: index + 1,
            visual,
            text,
            error: None,
        });
    }
    Ok(FileScore {
        expected_pages: their_count,
        pages,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn char_at(code: char, origin_x: f32, color: [u8; 3]) -> TextChar {
        TextChar {
            code: u32::from(code),
            origin_x,
            origin_y: 100.0,
            color,
        }
    }

    #[test]
    fn text_matches_by_code_position_and_colour() {
        let black = [0, 0, 0];
        let expected = [char_at('a', 10.0, black), char_at('b', 20.0, black)];
        assert_eq!(text_difference(&expected, &expected), 0.0);
        // Moved by more than the tolerance: both sides unmatched for that character.
        let moved = [char_at('a', 10.0, black), char_at('b', 22.0, black)];
        assert_eq!(text_difference(&moved, &expected), 0.5);
        // Recoloured: red instead of black.
        let recoloured = [char_at('a', 10.0, [255, 0, 0]), char_at('b', 20.0, black)];
        assert_eq!(text_difference(&recoloured, &expected), 0.5);
        assert_eq!(text_difference(&[], &[]), 0.0);
    }

    #[test]
    fn pixels_count_colour_differences() {
        let page = |rgb: [u8; 3]| PageCapture {
            width: 4,
            height: 4,
            pixels: rgb.repeat(16),
            chars: Vec::new(),
        };
        let white = page([255, 255, 255]);
        assert_eq!(page_difference(&white, &white).0, 0.0);
        // A blue that grey would equal in luminance still differs per channel.
        let mut tinted = white.clone();
        for pixel in tinted.pixels.chunks_exact_mut(3).take(8) {
            pixel.copy_from_slice(&[255, 255, 180]);
        }
        let (fraction, mask, mask_width, mask_height) = page_difference(&tinted, &white);
        assert_eq!((mask_width, mask_height), (2, 2));
        assert_eq!(mask, vec![true, true, false, false]);
        assert_eq!(fraction, 0.5);
    }

    #[test]
    fn capture_round_trips() {
        let capture = PageCapture {
            width: 1,
            height: 1,
            pixels: vec![1, 2, 3],
            chars: vec![char_at('x', 1.5, [4, 5, 6])],
        };
        assert_eq!(decode_capture(&encode_capture(&capture)), Some(capture));
        assert_eq!(decode_capture(&[0; 5]), None);
    }

    #[test]
    fn scores_pass_below_one_percent() {
        let page = PageScore {
            page: 1,
            visual: 0.009,
            text: 0.0,
            error: None,
        };
        assert!(page.passed());
        assert!(PageScore {
            text: 0.5,
            ..page.clone()
        }
        .passed());
        assert!(!PageScore {
            visual: 0.02,
            ..page.clone()
        }
        .passed());
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
