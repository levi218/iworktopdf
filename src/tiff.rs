//! Baseline TIFF decoder for images pasted into iWork documents (Skia has no TIFF codec). Handles
//! either byte order, strips or tiles, no/LZW/PackBits/Deflate compression with the horizontal
//! predictor, and 8- or 16-bit grey, palette, RGB and RGBA (associated or not). Returns `None` for
//! anything else, or for a truncated file, so the caller can fall back to a thumbnail.

use crate::bytes::{be_u16, be_u32, le_u16, le_u32};
use std::collections::HashMap;
use std::io::Read;

/// True when the bytes start with a TIFF header (either byte order).
pub fn is_tiff(bytes: &[u8]) -> bool {
    bytes.len() > 8 && (bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*"))
}

/// Decoded RGBA pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Row-major RGBA, 4 bytes per pixel.
    pub rgba: Vec<u8>,
    /// The alpha is associated (premultiplied), as the file's ExtraSamples tag says.
    pub premultiplied: bool,
}

/// Tag number → values of the first image file directory.
type Tags = HashMap<u32, Vec<u32>>;

/// Byte-order aware readers over the file.
struct Reader<'a> {
    bytes: &'a [u8],
    little_endian: bool,
}

impl Reader<'_> {
    fn u16(&self, offset: usize) -> Option<u32> {
        let value = if self.little_endian {
            le_u16(self.bytes, offset)
        } else {
            be_u16(self.bytes, offset)
        };
        value.map(u32::from)
    }

    fn u32(&self, offset: usize) -> Option<u32> {
        if self.little_endian {
            le_u32(self.bytes, offset)
        } else {
            be_u32(self.bytes, offset)
        }
    }

    /// One directory entry: its tag and values (inline when they fit in 4 bytes).
    fn entry(&self, entry: usize) -> Option<(u32, Vec<u32>)> {
        let (tag, field_type) = (self.u16(entry)?, self.u16(entry + 2)?);
        let count = self.u32(entry + 4)? as usize;
        let size = match field_type {
            1 | 2 | 7 => 1,
            3 => 2,
            _ => 4,
        };
        let at = if count * size <= 4 {
            entry + 8
        } else {
            self.u32(entry + 8)? as usize
        };
        let values = (0..count)
            .map(|index| match size {
                1 => self.bytes.get(at + index).map(|&byte| u32::from(byte)),
                2 => self.u16(at + 2 * index),
                _ => self.u32(at + 4 * index),
            })
            .collect::<Option<Vec<u32>>>()?;
        Some((tag, values))
    }

    /// The first image file directory.
    fn tags(&self) -> Option<Tags> {
        let directory = self.u32(4)? as usize;
        let count = self.u16(directory)? as usize;
        (0..count)
            .map(|index| self.entry(directory + 2 + index * 12))
            .collect()
    }
}

/// Decodes a baseline TIFF.
///
/// # Arguments
///
/// * `bytes` - The file's bytes
///
/// # Returns
///
/// The first image's pixels, or `None` when the layout is unsupported or the file is truncated.
pub fn decode(bytes: &[u8]) -> Option<Decoded> {
    if !is_tiff(bytes) {
        return None;
    }
    let reader = Reader {
        bytes,
        little_endian: bytes[0] == b'I',
    };
    let tags = reader.tags()?;
    let tag = |number: u32, default: u32| {
        tags.get(&number)
            .and_then(|values| values.first().copied())
            .unwrap_or(default)
    };
    let (width, height) = (tag(256, 0) as usize, tag(257, 0) as usize);
    let (samples_per_pixel, bits_per_sample) = (tag(277, 1) as usize, tag(258, 1) as usize);
    let photometric = tag(262, 2);
    let unsupported = width == 0 || height == 0 || tag(284, 1) != 1;
    if unsupported || (bits_per_sample != 8 && bits_per_sample != 16) {
        return None;
    }
    let bytes_per_pixel = samples_per_pixel * bits_per_sample / 8;
    let layout = Layout {
        width,
        height,
        bytes_per_pixel,
        compression: tag(259, 1),
        predictor: tag(317, 1) == 2 && bits_per_sample == 8,
    };
    let samples = assemble(&tags, bytes, &layout)?;
    let palette = tags.get(&320);
    // The high byte of a 16-bit sample comes second in little-endian files.
    let step = bits_per_sample / 8;
    let high_byte = usize::from(bits_per_sample == 16 && reader.little_endian);
    let mut rgba = Vec::with_capacity(width * height * 4);
    for pixel in samples.chunks_exact(bytes_per_pixel) {
        let sample = |index: usize| pixel.get(index * step + high_byte).copied();
        let alpha = |index: usize| {
            if samples_per_pixel > index {
                sample(index)
            } else {
                Some(255)
            }
        };
        let colour = match photometric {
            0 => sample(0).map(|value| [255 - value; 3]).zip(alpha(1)),
            1 => sample(0).map(|value| [value; 3]).zip(alpha(1)),
            3 => {
                let palette = palette?;
                let index = usize::from(if bits_per_sample == 8 {
                    pixel[0]
                } else {
                    sample(0)?
                });
                let entries = palette.len() / 3;
                let channel = |plane: usize| {
                    palette
                        .get(plane * entries + index)
                        .map(|&value| (value >> 8) as u8)
                };
                Some(([channel(0)?, channel(1)?, channel(2)?], 255))
            }
            2 => Some(([sample(0)?, sample(1)?, sample(2)?], alpha(3)?)),
            _ => None,
        };
        let ([red, green, blue], opacity) = colour?;
        rgba.extend_from_slice(&[red, green, blue, opacity]);
    }
    Some(Decoded {
        width,
        height,
        rgba,
        premultiplied: tag(338, 0) == 1,
    })
}

/// Image geometry and coding shared by every strip or tile.
struct Layout {
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    compression: u32,
    predictor: bool,
}

/// Decompresses every strip or tile into one row-major sample buffer.
fn assemble(tags: &Tags, bytes: &[u8], layout: &Layout) -> Option<Vec<u8>> {
    let row_bytes = layout.width * layout.bytes_per_pixel;
    let mut samples = vec![0u8; row_bytes * layout.height];
    let tiled = tags.contains_key(&322);
    let first = |number: u32| tags.get(&number).and_then(|values| values.first().copied());
    let (tile_width, tile_height) = if tiled {
        (first(322)? as usize, first(323)? as usize)
    } else {
        (
            layout.width,
            first(278).map_or(layout.height, |rows| rows as usize),
        )
    };
    if tile_width == 0 || tile_height == 0 {
        return None;
    }
    let none = Vec::new();
    let offsets = tags.get(&(if tiled { 324 } else { 273 })).unwrap_or(&none);
    let byte_counts = tags.get(&(if tiled { 325 } else { 279 })).unwrap_or(&none);
    let tiles_across = if tiled {
        layout.width.div_ceil(tile_width)
    } else {
        1
    };
    let tile_row_bytes = tile_width * layout.bytes_per_pixel;
    for (index, &offset) in offsets.iter().enumerate() {
        let offset = offset as usize;
        let available = bytes.len().saturating_sub(offset);
        let length = byte_counts
            .get(index)
            .map_or(available, |&count| count as usize);
        let expected = tile_row_bytes * tile_height;
        let mut chunk = inflate(layout.compression, bytes.get(offset..)?, length, expected)?;
        if layout.predictor {
            undo_horizontal_predictor(&mut chunk, tile_row_bytes, layout.bytes_per_pixel);
        }
        let (left, top) = if tiled {
            (
                index % tiles_across * tile_width,
                index / tiles_across * tile_height,
            )
        } else {
            (0, index * tile_height)
        };
        let copy = tile_row_bytes.min(layout.width.saturating_sub(left) * layout.bytes_per_pixel);
        for (row, source) in chunk.chunks(tile_row_bytes).take(tile_height).enumerate() {
            let (Some(source), true) = (source.get(..copy), top + row < layout.height) else {
                break;
            };
            let destination = (top + row) * row_bytes + left * layout.bytes_per_pixel;
            samples
                .get_mut(destination..destination + copy)?
                .copy_from_slice(source);
        }
    }
    Some(samples)
}

/// Horizontal differencing (predictor 2) on 8-bit samples: each sample adds the one to its left.
fn undo_horizontal_predictor(chunk: &mut [u8], row_bytes: usize, bytes_per_pixel: usize) {
    for row in chunk.chunks_exact_mut(row_bytes) {
        for index in bytes_per_pixel..row.len() {
            row[index] = row[index].wrapping_add(row[index - bytes_per_pixel]);
        }
    }
}

/// Decompresses one strip or tile: none (1), LZW (5), Deflate (8, 32946) or PackBits (32773).
fn inflate(compression: u32, bytes: &[u8], length: usize, expected: usize) -> Option<Vec<u8>> {
    let source = &bytes[..length.min(bytes.len())];
    match compression {
        1 => Some(source.to_vec()),
        8 | 32946 => {
            let mut output = Vec::with_capacity(expected);
            flate2::read::ZlibDecoder::new(source)
                .read_to_end(&mut output)
                .ok()?;
            Some(output)
        }
        32773 => Some(pack_bits(source, expected)),
        5 => Some(lzw(source, expected)),
        _ => None,
    }
}

/// PackBits run-length decoding.
fn pack_bits(source: &[u8], expected: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(expected);
    let mut position = 0;
    while let Some(&header) = source.get(position) {
        let header = header as i8;
        position += 1;
        if header >= 0 {
            // A literal run of header + 1 bytes.
            let count = (header as usize + 1).min(source.len() - position);
            output.extend_from_slice(&source[position..position + count]);
            position += count;
        } else if header != -128 {
            // One byte repeated 1 - header times.
            let Some(&value) = source.get(position) else {
                break;
            };
            position += 1;
            output.extend(std::iter::repeat_n(value, (1 - i32::from(header)) as usize));
        }
    }
    output
}

/// TIFF LZW (MSB-first codes, early change).
fn lzw(source: &[u8], expected: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(expected);
    let reset = |table: &mut Vec<Vec<u8>>| {
        table.clear();
        table.extend((0..=255u8).map(|byte| vec![byte]));
        table.extend([vec![], vec![]]); // 256 clear, 257 end of information
    };
    let mut table: Vec<Vec<u8>> = Vec::with_capacity(4096);
    reset(&mut table);
    let (mut bit, end_bit, mut code_width) = (0usize, source.len() * 8, 9usize);
    let mut previous: Option<Vec<u8>> = None;
    while bit + code_width <= end_bit {
        let code = (0..code_width).fold(0usize, |code, offset| {
            let at = bit + offset;
            code << 1 | usize::from(source[at >> 3] >> (7 - (at & 7)) & 1)
        });
        bit += code_width;
        if code == 257 {
            break;
        }
        if code == 256 {
            reset(&mut table);
            code_width = 9;
            previous = None;
            continue;
        }
        let entry = match (table.get(code), &previous) {
            (Some(entry), _) => entry.clone(),
            (None, Some(prior)) => [prior.as_slice(), &prior[..1]].concat(),
            (None, None) => break,
        };
        output.extend_from_slice(&entry);
        if let Some(prior) = &previous {
            table.push([prior.as_slice(), &entry[..1]].concat());
        }
        previous = Some(entry);
        // Early change: the code width grows one code before the table fills.
        if table.len() + 1 >= 1 << code_width && code_width < 12 {
            code_width += 1;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A little-endian 2x1 RGB TIFF with one uncompressed strip.
    fn rgb_tiff() -> Vec<u8> {
        let entries: [(u16, u16, u32); 7] = [
            (256, 3, 2),
            (257, 3, 1),
            (258, 3, 8),
            (262, 3, 2),
            (273, 4, 98),
            (277, 3, 3),
            (279, 4, 6),
        ];
        let mut file = b"II*\0".to_vec();
        file.extend(8u32.to_le_bytes());
        file.extend((entries.len() as u16).to_le_bytes());
        for (tag, field_type, value) in entries {
            file.extend(tag.to_le_bytes());
            file.extend(field_type.to_le_bytes());
            file.extend(1u32.to_le_bytes());
            file.extend(value.to_le_bytes());
        }
        file.extend(0u32.to_le_bytes());
        assert_eq!(file.len(), 98);
        file.extend([255, 0, 0, 0, 0, 255]);
        file
    }

    #[test]
    fn decodes_uncompressed_rgb() {
        let image = decode(&rgb_tiff()).expect("a valid TIFF decodes");
        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(image.rgba, vec![255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn truncated_file_is_none() {
        let file = rgb_tiff();
        assert!(decode(&file[..60]).is_none());
        assert!(!is_tiff(b"PNG....."));
    }

    #[test]
    fn pack_bits_runs() {
        // literal "ab", then 'c' repeated 3 times
        assert_eq!(pack_bits(&[1, b'a', b'b', 0xFE, b'c'], 5), b"abccc");
    }

    #[test]
    fn lzw_decodes_a_repeated_pair() {
        // Codes (9-bit): 256 clear, 'a', 'b', 258 ("ab"), 257 end.
        let bits: Vec<u16> = [256u16, 97, 98, 258, 257]
            .iter()
            .flat_map(|code| (0..9).rev().map(move |shift| (code >> shift) & 1))
            .collect();
        let packed: Vec<u8> = bits
            .chunks(8)
            .map(|byte| {
                byte.iter()
                    .enumerate()
                    .fold(0u8, |acc, (index, &bit)| acc | (bit as u8) << (7 - index))
            })
            .collect();
        assert_eq!(lzw(&packed, 4), b"abab");
    }
}
