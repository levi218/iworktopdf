//! WebAssembly entry point (target `wasm32-unknown-emscripten`; Skia cannot link to plain WASM).
//!
//! JavaScript copies the document into the module's memory and calls `font_names` or `convert`;
//! the result (or an error message) then sits in an output buffer read through `output_ptr` /
//! `output_len`. Font metrics and Nimbus Sans are embedded under /res (see build.rs); font files
//! the page supplies (installed on the device, or picked by the user) are written to /fonts before
//! the first conversion, since the library indexes them once.

#[cfg(target_os = "emscripten")]
mod sjlj;

use std::cell::RefCell;
use std::panic::{self, AssertUnwindSafe};

use iwork2pdf::iwa::IwaDocument;
use iwork2pdf::render::{self, DocumentKind};
use iwork2pdf::{shaper, styles};

/// Where the page writes supplied font files (searched recursively by PostScript name).
const USER_FONT_DIR: &str = "/fonts";

thread_local! {
    /// The last call's result bytes, or its UTF-8 error message. `thread_local!` gives a
    /// global without locking; the module runs on one thread.
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Converts a single-file iWork document held in memory to PDF.
///
/// # Arguments
///
/// * `document` - The .numbers, .key or .pages file's bytes
/// * `extension` - The file's extension without the dot (any case), which selects the layout rules
///
/// # Returns
///
/// The PDF's bytes and its page count.
///
/// # Errors
///
/// A readable message when the extension is not an iWork one or the document cannot be read.
fn convert_bytes(document: &[u8], extension: &str) -> Result<(Vec<u8>, usize), String> {
    let kind = DocumentKind::from_extension(extension)
        .ok_or_else(|| format!("unsupported document type '.{extension}'"))?;
    let decoded = IwaDocument::from_bytes(document).map_err(|error| error.to_string())?;
    render::render(decoded, kind).map_err(|error| error.to_string())
}

/// The fonts worth supplying for a document held in memory: the ones its styles name (supply
/// their whole families: bold and italic variants are picked by flag), then the fallback faces
/// the renderer may substitute (supply exactly those).
///
/// # Arguments
///
/// * `document` - The .numbers, .key or .pages file's bytes
///
/// # Returns
///
/// The named fonts one per line, an empty line, then the fallback faces one per line; and how
/// many names the document's styles use.
///
/// # Errors
///
/// A readable message when the document cannot be read.
fn font_names_text(document: &[u8]) -> Result<(Vec<u8>, usize), String> {
    let decoded = IwaDocument::from_bytes(document).map_err(|error| error.to_string())?;
    let named = styles::font_names(&decoded);
    let fallbacks = shaper::document_fallback_faces(&decoded);
    let text = format!("{}\n\n{}", named.join("\n"), fallbacks.join("\n"));
    Ok((text.into_bytes(), named.len()))
}

/// Runs `operation` on `length` bytes at `data`, storing its output (or error message) for
/// `output_ptr` / `output_len`.
///
/// # Returns
///
/// The count the operation reports, or -1 on failure.
///
/// # Safety
///
/// `data` must point to `length` readable bytes, valid for the duration of the call.
unsafe fn run(
    data: *const u8,
    length: usize,
    operation: impl FnOnce(&[u8]) -> Result<(Vec<u8>, usize), String>,
) -> i32 {
    // SAFETY: the caller guarantees `data` per the contract above.
    let input = unsafe { std::slice::from_raw_parts(data, length) };
    // A panic would otherwise abort the whole module; report it like any other failure.
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| operation(input)))
        .unwrap_or_else(|_| Err("the converter crashed on this document".to_owned()));
    let (bytes, count) = match outcome {
        Ok((bytes, count)) => (bytes, i32::try_from(count).unwrap_or(i32::MAX)),
        Err(message) => (message.into_bytes(), -1),
    };
    OUTPUT.with(|output| *output.borrow_mut() = bytes);
    count
}

/// Converts `length` bytes at `data` (an iWork document with extension `extension`) to PDF.
///
/// # Arguments
///
/// * `data` - The document's bytes in module memory (allocated by the caller with `_malloc`)
/// * `length` - How many bytes `data` holds
/// * `extension` - A NUL-terminated extension ("numbers", "key" or "pages")
///
/// # Returns
///
/// The page count, with the PDF in the output buffer; -1 on failure, with the error message
/// (UTF-8) in the output buffer instead.
///
/// # Safety
///
/// `data` must point to `length` readable bytes and `extension` to a NUL-terminated string,
/// both valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn convert(
    data: *const u8,
    length: usize,
    extension: *const std::ffi::c_char,
) -> i32 {
    // SAFETY: the caller guarantees `extension` per the contract above.
    let extension = unsafe { std::ffi::CStr::from_ptr(extension) }.to_string_lossy();
    // SAFETY: forwarded contract.
    unsafe { run(data, length, |document| convert_bytes(document, &extension)) }
}

/// Lists the PostScript font names worth supplying for `length` bytes at `data` (an iWork
/// document), so the page can fetch those fonts before converting.
///
/// # Arguments
///
/// * `data` - The document's bytes in module memory (allocated by the caller with `_malloc`)
/// * `length` - How many bytes `data` holds
///
/// # Returns
///
/// How many fonts the document's styles name, with the names in the output buffer (UTF-8, see
/// `font_names_text`); -1 on failure, with the error message in the output buffer instead.
///
/// # Safety
///
/// `data` must point to `length` readable bytes, valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn font_names(data: *const u8, length: usize) -> i32 {
    // SAFETY: forwarded contract.
    unsafe { run(data, length, font_names_text) }
}

/// Where the last call's output starts in module memory.
#[no_mangle]
pub extern "C" fn output_ptr() -> *const u8 {
    OUTPUT.with(|output| output.borrow().as_ptr())
}

/// The length in bytes of the last call's output.
#[no_mangle]
pub extern "C" fn output_len() -> usize {
    OUTPUT.with(|output| output.borrow().len())
}

/// Runs once when the module is instantiated: points the font lookup at the embedded files and
/// at the (initially empty) folder for supplied fonts.
fn main() {
    std::env::set_var("IWORK2PDF_RESOURCES", "/res");
    // Nimbus Sans stands in for Helvetica Neue (the only lookalike embedded).
    std::env::set_var("IWORK2PDF_HN", "nimbus");
    std::env::set_var("IWORK2PDF_FONTS", USER_FONT_DIR);
    if let Err(error) = std::fs::create_dir_all(USER_FONT_DIR) {
        // Supplied fonts then cannot be written; conversion still works with Nimbus Sans.
        eprintln!("cannot create {USER_FONT_DIR}: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_extension() {
        let error = convert_bytes(b"", "docx").unwrap_err();
        assert_eq!(error, "unsupported document type '.docx'");
    }

    #[test]
    fn reports_unreadable_document() {
        let error = convert_bytes(b"not a zip", "pages").unwrap_err();
        assert!(error.starts_with("zip:"), "{error}");
        let error = font_names_text(b"not a zip").unwrap_err();
        assert!(error.starts_with("zip:"), "{error}");
    }

    #[test]
    fn run_stores_output_or_error() {
        let data = b"abc";
        let count = unsafe { run(data.as_ptr(), data.len(), |input| Ok((input.to_vec(), 2))) };
        assert_eq!(count, 2);
        assert_eq!(OUTPUT.with(|output| output.borrow().clone()), b"abc");
        let count = unsafe { run(data.as_ptr(), data.len(), |_| Err("bad".to_owned())) };
        assert_eq!(count, -1);
        assert_eq!(OUTPUT.with(|output| output.borrow().clone()), b"bad");
    }
}
