//! PDFium (the same build PDFtoImage ships) for rasterising PDFs: pasted vector artwork in
//! documents and both sides of the acceptance comparison.

use pdfium_render::prelude::*;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// One process-wide binding behind a lock. pdfium-render's `sync` feature only serialises single
/// calls, but PDFium's shared objects (colour spaces, fonts) use non-atomic reference counts, so
/// two threads each loading a document still corrupt memory. Holding the lock for a whole
/// load-render-drop session prevents that.
static PDFIUM: OnceLock<Option<Mutex<Pdfium>>> = OnceLock::new();

/// Library search: IWORK2PDF_PDFIUM (a file), next to the executable, the working directory,
/// then the system library path.
fn bind() -> Option<Pdfium> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(path) = std::env::var("IWORK2PDF_PDFIUM") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|path| path.to_path_buf()))
    {
        candidates.push(Pdfium::pdfium_platform_library_name_at_path(&dir));
    }
    candidates.push(Pdfium::pdfium_platform_library_name_at_path("./"));
    for candidate in candidates {
        if candidate.exists() {
            if let Ok(bindings) = Pdfium::bind_to_library(&candidate) {
                return Some(Pdfium::new(bindings));
            }
        }
    }
    Pdfium::bind_to_system_library().ok().map(Pdfium::new)
}

/// Exclusive access to the PDFium binding, or `None` when the library is not found.
///
/// Documents loaded through the guard borrow it, so the compiler guarantees they are dropped
/// before the lock is released (Rust's borrow checker plays the role of a `with` block here).
///
/// # Returns
///
/// A lock guard that dereferences to `Pdfium`; other threads wait until it is dropped.
pub fn get() -> Option<MutexGuard<'static, Pdfium>> {
    let binding = PDFIUM.get_or_init(|| bind().map(Mutex::new)).as_ref()?;
    // A panic while rendering leaves the lock "poisoned"; PDFium itself is still usable.
    Some(binding.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Renders a page at `dpi` onto `background` (None: transparent). RGBA, unpremultiplied.
pub fn render(
    page: &PdfPage,
    dpi: f32,
    background: Option<PdfColor>,
) -> Result<(usize, usize, Vec<u8>), String> {
    let scale = dpi / 72.0;
    let (width, height) = (
        (page.width().value * scale).round() as i32,
        (page.height().value * scale).round() as i32,
    );
    let cfg = PdfRenderConfig::new()
        .render_annotations(true)
        .render_form_data(true)
        .set_target_size(width.max(1), height.max(1))
        .set_clear_color(background.unwrap_or(PdfColor::new(0, 0, 0, 0)));
    let bitmap = page
        .render_with_config(&cfg)
        .map_err(|exe| exe.to_string())?;
    Ok((
        bitmap.width() as usize,
        bitmap.height() as usize,
        bitmap.as_rgba_bytes(),
    ))
}
