//! iWork 2013+ containers: Snappy-framed IWA archives of protobuf messages, read without a schema
//! by field number (field numbers come from the IWA .proto definitions in IWorkToPdf/schema/).

use crate::bytes::{le_u32, le_u64};
use crate::error::{Error, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::rc::Rc;

/// One protobuf field: its number, wire type and either a scalar value or a byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// The field number from the .proto definition.
    pub number: i32,
    /// Wire type: 0 varint, 1 64-bit, 2 length-delimited, 5 32-bit.
    pub wire: u8,
    /// The raw scalar (varint or fixed-width bits); 0 for length-delimited fields.
    pub value: u64,
    /// Byte range of a length-delimited field within the message buffer.
    start: usize,
    len: usize,
}

/// A schema-less protobuf message: fields kept by number in wire order, interpreted on demand.
///
/// The buffer is shared (`Rc` is a reference-counted pointer, like a Python object reference), so
/// sub-messages are cheap views into the same bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Proto {
    data: Rc<Vec<u8>>,
    fields: Vec<Field>,
}

/// Reads a base-128 varint at `*position`, advancing it past the varint (truncated input stops).
///
/// # Arguments
///
/// * `buffer` - The bytes
/// * `position` - Read position, updated in place (`&mut` lets the callee modify the caller's
///   variable)
///
/// # Returns
///
/// The decoded value.
pub fn read_varint(buffer: &[u8], position: &mut usize) -> u64 {
    let mut value = 0u64;
    let mut shift = 0;
    while let Some(&byte) = buffer.get(*position) {
        *position += 1;
        if shift < 64 {
            value |= u64::from(byte & 0x7f) << shift;
        }
        if byte < 0x80 {
            break;
        }
        shift += 7;
    }
    value
}

impl Proto {
    /// An empty message (every accessor returns its default).
    pub fn empty() -> Proto {
        Proto {
            data: Rc::new(Vec::new()),
            fields: Vec::new(),
        }
    }

    /// Parses the message at `data[start..start + len]`; a malformed tail is ignored.
    ///
    /// # Arguments
    ///
    /// * `data` - The shared buffer holding the message
    /// * `start` - Offset of the message
    /// * `len` - Length of the message
    ///
    /// # Returns
    ///
    /// The message with its fields indexed.
    pub fn new(data: Rc<Vec<u8>>, start: usize, len: usize) -> Proto {
        let end = start.saturating_add(len).min(data.len());
        let buffer = &data[..end];
        let mut fields = Vec::new();
        let mut position = start.min(end);
        while position < end {
            let key = read_varint(buffer, &mut position);
            let (number, wire) = ((key >> 3) as i32, (key & 7) as u8);
            let (value, field_start, field_len, width) = match wire {
                0 => (read_varint(buffer, &mut position), 0, 0, 0),
                1 => match le_u64(buffer, position) {
                    Some(value) => (value, 0, 0, 8),
                    None => break,
                },
                5 => match le_u32(buffer, position) {
                    Some(value) => (u64::from(value), 0, 0, 4),
                    None => break,
                },
                2 => {
                    let declared = read_varint(buffer, &mut position) as usize;
                    let available = declared.min(end.saturating_sub(position));
                    (0, position, available, available)
                }
                // Groups (3, 4) and unknown wire types: stop at what was read.
                _ => break,
            };
            position += width;
            fields.push(Field {
                number,
                wire,
                value,
                start: field_start,
                len: field_len,
            });
        }
        Proto { data, fields }
    }

    /// All fields in wire order.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// True when the message has no fields.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    fn bytes(&self, field: &Field) -> &[u8] {
        &self.data[field.start..field.start + field.len]
    }

    fn sub(&self, field: &Field) -> Proto {
        Proto::new(Rc::clone(&self.data), field.start, field.len)
    }

    /// True when field `number` is present.
    pub fn has(&self, number: i32) -> bool {
        self.fields.iter().any(|field| field.number == number)
    }

    /// The last occurrence of a field (protobuf: the last value of a scalar wins).
    fn get(&self, number: i32) -> Option<&Field> {
        self.fields
            .iter()
            .rev()
            .find(|field| field.number == number)
    }

    fn occurrences(&self, number: i32) -> impl Iterator<Item = &Field> {
        self.fields
            .iter()
            .filter(move |field| field.number == number)
    }

    /// A field as an unsigned integer (0 when absent).
    pub fn uint(&self, number: i32) -> u64 {
        self.get(number).map_or(0, |field| field.value)
    }

    /// A field as a signed 32-bit integer (0 when absent).
    pub fn int(&self, number: i32) -> i32 {
        self.int_or(number, 0)
    }

    /// A field as a signed 32-bit integer, or `default` when absent.
    pub fn int_or(&self, number: i32, default: i32) -> i32 {
        self.get(number)
            .map_or(default, |field| field.value as i64 as i32)
    }

    /// A field as a boolean (false when absent).
    pub fn boolean(&self, number: i32) -> bool {
        self.boolean_or(number, false)
    }

    /// A field as a boolean, or `default` when absent.
    pub fn boolean_or(&self, number: i32, default: bool) -> bool {
        self.get(number).map_or(default, |field| field.value != 0)
    }

    /// A field as a 32-bit float (0 when absent).
    pub fn float(&self, number: i32) -> f32 {
        self.float_or(number, 0.0)
    }

    /// A field as a 32-bit float, or `default` when absent.
    pub fn float_or(&self, number: i32, default: f32) -> f32 {
        self.get(number)
            .map_or(default, |field| f32::from_bits(field.value as u32))
    }

    /// A field as a 64-bit float (0 when absent).
    pub fn double(&self, number: i32) -> f64 {
        self.double_or(number, 0.0)
    }

    /// A field as a 64-bit float, or `default` when absent.
    pub fn double_or(&self, number: i32, default: f64) -> f64 {
        self.get(number)
            .map_or(default, |field| f64::from_bits(field.value))
    }

    /// A field as UTF-8 text (invalid sequences replaced), if present.
    pub fn string(&self, number: i32) -> Option<String> {
        self.get(number)
            .map(|field| String::from_utf8_lossy(self.bytes(field)).into_owned())
    }

    /// The raw bytes of a length-delimited field (empty when absent).
    pub fn raw(&self, number: i32) -> &[u8] {
        self.get(number).map_or(&[], |field| self.bytes(field))
    }

    /// A sub-message (empty when absent).
    pub fn message(&self, number: i32) -> Proto {
        self.message_opt(number).unwrap_or_else(Proto::empty)
    }

    /// A sub-message, if present.
    pub fn message_opt(&self, number: i32) -> Option<Proto> {
        self.get(number).map(|field| self.sub(field))
    }

    /// Every occurrence of a repeated sub-message.
    pub fn messages(&self, number: i32) -> Vec<Proto> {
        self.occurrences(number)
            .map(|field| self.sub(field))
            .collect()
    }

    /// Every occurrence of a repeated string.
    pub fn strings(&self, number: i32) -> Vec<String> {
        let text = |field: &Field| String::from_utf8_lossy(self.bytes(field)).into_owned();
        self.occurrences(number).map(text).collect()
    }

    /// A repeated integer, packed or not.
    pub fn uints(&self, number: i32) -> Vec<u64> {
        let mut values = Vec::new();
        for field in self.occurrences(number) {
            if field.wire != 2 {
                values.push(field.value);
                continue;
            }
            let packed = self.bytes(field);
            let mut position = 0;
            while position < packed.len() {
                values.push(read_varint(packed, &mut position));
            }
        }
        values
    }

    /// A repeated float, packed or not.
    pub fn floats(&self, number: i32) -> Vec<f32> {
        let mut values = Vec::new();
        for field in self.occurrences(number) {
            if field.wire == 2 {
                let words = self.bytes(field).chunks_exact(4);
                values.extend(words.filter_map(|word| le_u32(word, 0)).map(f32::from_bits));
            } else {
                values.push(f32::from_bits(field.value as u32));
            }
        }
        values
    }

    /// A TSP.Reference field (`{ uint64 identifier = 1 }`): the referenced object's id, 0 when
    /// absent.
    pub fn reference(&self, number: i32) -> u64 {
        self.get(number).map_or(0, |field| self.sub(field).uint(1))
    }

    /// Every TSP.Reference in a repeated field.
    pub fn references(&self, number: i32) -> Vec<u64> {
        self.occurrences(number)
            .map(|field| self.sub(field).uint(1))
            .collect()
    }
}

/// Decodes the IWA framing: chunks of a 1-byte type and a 3-byte little-endian length, each a raw
/// (unframed, no CRC) Snappy block.
///
/// # Arguments
///
/// * `data` - The .iwa file's bytes
///
/// # Returns
///
/// The concatenated decompressed chunks; empty or truncated chunks end the archive.
pub fn decode_iwa(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len() * 4);
    let mut position = 0;
    while let Some(header) = data.get(position..position + 4) {
        let chunk_len =
            usize::from(header[1]) | usize::from(header[2]) << 8 | usize::from(header[3]) << 16;
        position += 4;
        if chunk_len == 0 {
            continue;
        }
        let Some(chunk) = data.get(position..position + chunk_len) else {
            break;
        };
        snappy_raw(chunk, &mut output);
        position += chunk_len;
    }
    output
}

/// Appends one raw Snappy block's output to `output`; a corrupt block stops early.
fn snappy_raw(block: &[u8], output: &mut Vec<u8>) {
    let mut position = 0;
    let total = read_varint(block, &mut position) as usize;
    let base = output.len();
    output.reserve(total);
    while let Some(&tag) = block.get(position) {
        position += 1;
        let (copy_len, offset) = match tag & 3 {
            0 => {
                // Literal: length in the tag, or in the next 1-4 bytes.
                let mut literal_len = usize::from(tag >> 2);
                if literal_len >= 60 {
                    let size_bytes = literal_len - 59;
                    let Some(size) = block.get(position..position + size_bytes) else {
                        return;
                    };
                    literal_len = size
                        .iter()
                        .rev()
                        .fold(0, |acc, &byte| acc << 8 | usize::from(byte));
                    position += size_bytes;
                }
                let Some(literal) = block.get(position..position + literal_len + 1) else {
                    return;
                };
                output.extend_from_slice(literal);
                position += literal_len + 1;
                continue;
            }
            1 => {
                let Some(&low) = block.get(position) else {
                    return;
                };
                position += 1;
                (
                    usize::from((tag >> 2) & 7) + 4,
                    usize::from(tag >> 5) << 8 | usize::from(low),
                )
            }
            2 => {
                let Some(offset) = block.get(position..position + 2) else {
                    return;
                };
                position += 2;
                (
                    usize::from(tag >> 2) + 1,
                    usize::from(offset[0]) | usize::from(offset[1]) << 8,
                )
            }
            _ => {
                let Some(offset) = le_u32(block, position) else {
                    return;
                };
                position += 4;
                (usize::from(tag >> 2) + 1, offset as usize)
            }
        };
        if offset == 0 || offset > output.len() - base {
            return;
        }
        // Overlapping copies are legal (run-length style), so copy byte by byte.
        for _ in 0..copy_len {
            output.push(output[output.len() - offset]);
        }
    }
}

/// One archive object: identifier, message type and body.
#[derive(Debug, Clone, PartialEq)]
pub struct IwaObject {
    /// The object identifier other objects reference.
    pub id: u64,
    /// The message type (see the TSPRegistry maps in IWorkToPdf/schema/*.json).
    pub typ: u32,
    /// The message.
    pub body: Proto,
}

impl IwaObject {
    /// The same object read as another type with another body (wrapped archives such as
    /// KN.PlaceholderArchive around a TSWP.ShapeInfoArchive).
    ///
    /// # Arguments
    ///
    /// * `typ` - The inner message's type
    /// * `body` - The inner message
    ///
    /// # Returns
    ///
    /// A copy with the same identifier.
    pub fn with(&self, typ: u32, body: Proto) -> IwaObject {
        IwaObject {
            id: self.id,
            typ,
            body,
        }
    }
}

/// Package files by relative path.
type Files = HashMap<String, Rc<Vec<u8>>>;

/// An iWork 2013+ document: every object of every Index/*.iwa archive by identifier, plus the
/// package files (Data/...) by name. Accepts a single-file (zip) document or a package directory
/// (with Index.zip or a loose Index/ folder).
#[derive(Debug)]
pub struct IwaDocument {
    objects: HashMap<u64, IwaObject>,
    files: Files,
    /// Data identifier → file name (TSP.PackageMetadata), built on first use.
    data_names: RefCell<Option<HashMap<u64, String>>>,
}

/// Reads every file of a zip archive into `files`.
///
/// `package` strips a common package-folder prefix ("Presentation.key/Index.zip") and opens a
/// nested Index.zip.
fn read_zip(bytes: &[u8], package: bool, files: &mut Files) -> Result<()> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let names: Vec<String> = archive
        .file_names()
        .filter(|name| !name.ends_with('/'))
        .map(str::to_owned)
        .collect();
    let prefix = if package { package_prefix(&names) } else { "" };
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let relative = entry
            .name()
            .get(prefix.len()..)
            .unwrap_or_default()
            .to_owned();
        let mut contents = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut contents)
            .map_err(Error::io(Path::new(&relative)))?;
        if package && relative == "Index.zip" {
            read_zip(&contents, false, files)?;
        } else {
            files.insert(relative, Rc::new(contents));
        }
    }
    Ok(())
}

/// The folder prefix shared by every entry of a zipped package folder, if it holds an index.
fn package_prefix(names: &[String]) -> &str {
    let Some(first) = names.first() else {
        return "";
    };
    let Some(slash) = first.find('/') else {
        return "";
    };
    let prefix = &first[..=slash];
    let shared = names.iter().all(|name| name.starts_with(prefix));
    let indexed = names.iter().any(|name| {
        let rest = name.strip_prefix(prefix);
        rest.is_some_and(|rest| rest == "Index.zip" || rest.starts_with("Index/"))
    });
    if shared && indexed {
        prefix
    } else {
        ""
    }
}

/// Reads a package directory's files (relative paths with '/') into `files`.
fn read_dir_files(root: &Path, dir: &Path, files: &mut Files) -> Result<()> {
    for entry in std::fs::read_dir(dir).map_err(Error::io(dir))? {
        let path = entry.map_err(Error::io(dir))?.path();
        if path.is_dir() {
            read_dir_files(root, &path, files)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let contents = std::fs::read(&path).map_err(Error::io(&path))?;
        if relative.eq_ignore_ascii_case("Index.zip") {
            read_zip(&contents, false, files)?;
        } else {
            files.insert(relative, Rc::new(contents));
        }
    }
    Ok(())
}

impl IwaDocument {
    /// Opens a document.
    ///
    /// # Arguments
    ///
    /// * `path` - A single-file document or a package directory
    ///
    /// # Returns
    ///
    /// The document with every archive object decoded.
    ///
    /// # Errors
    ///
    /// Returns `Error::Io` or `Error::Zip` when the files cannot be read, and `Error::NotIwa` when
    /// the package holds no IWA archives.
    pub fn open(path: &Path) -> Result<IwaDocument> {
        let mut files = HashMap::new();
        if path.is_dir() {
            read_dir_files(path, path, &mut files)?;
        } else {
            read_zip(
                &std::fs::read(path).map_err(Error::io(path))?,
                true,
                &mut files,
            )?;
        }
        let objects = load_archives(&files)?;
        Ok(IwaDocument {
            objects,
            files,
            data_names: RefCell::new(None),
        })
    }

    /// The object with identifier `id` (0 is "no object").
    pub fn get(&self, id: u64) -> Option<&IwaObject> {
        if id == 0 {
            None
        } else {
            self.objects.get(&id)
        }
    }

    /// The body of object `id`, if it exists.
    pub fn body(&self, id: u64) -> Option<Proto> {
        self.get(id).map(|object| object.body.clone())
    }

    /// Objects of a message type, in identifier order (deterministic).
    pub fn of_type(&self, typ: u32) -> Vec<&IwaObject> {
        let mut found: Vec<&IwaObject> = self
            .objects
            .values()
            .filter(|object| object.typ == typ)
            .collect();
        found.sort_unstable_by_key(|object| object.id);
        found
    }

    /// A package file by relative path (case-insensitive, as on Apple's file systems).
    pub fn read_file(&self, relative: &str) -> Option<Rc<Vec<u8>>> {
        let folded = || {
            self.files
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(relative))
        };
        let found = self
            .files
            .get(relative)
            .or_else(|| folded().map(|(_, contents)| contents));
        found.cloned()
    }

    /// Data file bytes by data identifier (TSP.PackageMetadata in Metadata.iwa).
    ///
    /// # Arguments
    ///
    /// * `data_id` - A TSP.DataReference identifier
    ///
    /// # Returns
    ///
    /// The file's bytes, or `None` when the package does not carry it.
    pub fn read_data(&self, data_id: u64) -> Option<Rc<Vec<u8>>> {
        let mut names = self.data_names.borrow_mut();
        let names = names.get_or_insert_with(|| {
            let metadata = self.of_type(11006);
            let infos = metadata
                .into_iter()
                .flat_map(|object| object.body.messages(4));
            let named =
                |info: Proto| Some((info.uint(1), info.string(4).or_else(|| info.string(3))?));
            infos.filter_map(named).collect()
        });
        let file = names.get(&data_id)?;
        self.read_file(&format!("Data/{file}"))
            .or_else(|| self.read_file(file))
    }
}

/// Decodes every Index/*.iwa archive into objects by identifier.
fn load_archives(files: &Files) -> Result<HashMap<u64, IwaObject>> {
    let mut archives: Vec<(&String, &Rc<Vec<u8>>)> = files
        .iter()
        .filter(|(name, _)| name.to_ascii_lowercase().ends_with(".iwa"))
        .collect();
    if archives.is_empty() {
        return Err(Error::NotIwa);
    }
    archives.sort_unstable_by_key(|(name, _)| *name);
    let mut objects = HashMap::new();
    for (_, contents) in archives {
        let data = Rc::new(decode_iwa(contents));
        let mut position = 0;
        while position < data.len() {
            let info_len = read_varint(&data, &mut position) as usize;
            let info = Proto::new(Rc::clone(&data), position, info_len); // TSP.ArchiveInfo
            position += info_len;
            let id = info.uint(1);
            // TSP.MessageInfo per message; the first is the object, the rest are diffs.
            for (index, message_info) in info.messages(2).iter().enumerate() {
                let message_len = message_info.uint(3) as usize;
                if index == 0 {
                    let body = Proto::new(Rc::clone(&data), position.min(data.len()), message_len);
                    let typ = message_info.uint(1) as u32;
                    objects.insert(id, IwaObject { id, typ, body });
                }
                position += message_len;
            }
        }
    }
    Ok(objects)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_varint(mut value: u64, output: &mut Vec<u8>) {
        while value >= 0x80 {
            output.push((value as u8) | 0x80);
            value >>= 7;
        }
        output.push(value as u8);
    }

    fn parse(bytes: &[u8]) -> Proto {
        Proto::new(Rc::new(bytes.to_vec()), 0, bytes.len())
    }

    #[test]
    fn varint_round_trip() {
        let mut encoded = Vec::new();
        encode_varint(300, &mut encoded);
        let mut position = 0;
        assert_eq!(read_varint(&encoded, &mut position), 300);
        assert_eq!(position, 2);
    }

    #[test]
    fn proto_reads_scalars_strings_and_messages() {
        // field 1 varint 150; field 2 "hi"; field 3 { field 1 = 7 }; field 4 float 1.5
        let mut bytes = vec![
            0x08, 0x96, 0x01, 0x12, 2, b'h', b'i', 0x1a, 2, 0x08, 7, 0x25,
        ];
        bytes.extend_from_slice(&1.5f32.to_le_bytes());
        let message = parse(&bytes);
        assert_eq!(message.uint(1), 150);
        assert_eq!(message.string(2).as_deref(), Some("hi"));
        assert_eq!(message.reference(3), 7);
        assert_eq!(message.float(4), 1.5);
        assert!(!message.has(9));
        assert_eq!(message.int_or(9, -1), -1);
        assert!(message.message(9).is_empty());
    }

    #[test]
    fn proto_reads_packed_repeated() {
        assert_eq!(parse(&[0x0a, 3, 1, 2, 3]).uints(1), vec![1, 2, 3]);
    }

    #[test]
    fn truncated_proto_keeps_what_was_read() {
        let message = parse(&[0x08, 1, 0x11, 1, 2]);
        assert_eq!(message.uint(1), 1);
        assert!(!message.has(2));
    }

    #[test]
    fn snappy_literal_and_overlapping_copy() {
        // Uncompressed length 6; literal "ab"; copy (kind 1) length 4 at offset 2 → "ababab".
        let block = [6, 0x04, b'a', b'b', 0x01, 2];
        let mut framed = vec![0, block.len() as u8, 0, 0];
        framed.extend_from_slice(&block);
        assert_eq!(decode_iwa(&framed), b"ababab");
    }

    #[test]
    fn snappy_bad_offset_stops() {
        let mut output = Vec::new();
        snappy_raw(&[4, 0x00, b'a', 0x05, 9], &mut output);
        assert_eq!(output, b"a");
    }

    #[test]
    fn package_prefix_needs_an_index() {
        let with_index = vec![
            "X.key/Index.zip".to_string(),
            "X.key/Data/a.png".to_string(),
        ];
        let without_index = vec!["X.key/a.txt".to_string(), "X.key/b.txt".to_string()];
        assert_eq!(package_prefix(&with_index), "X.key/");
        assert_eq!(package_prefix(&without_index), "");
    }

    #[test]
    fn not_an_iwa_package() {
        let files = HashMap::from([("a.txt".to_string(), Rc::new(vec![1]))]);
        assert!(matches!(load_archives(&files), Err(Error::NotIwa)));
    }
}
