//! PDFium (the same build PDFtoImage ships) for rasterising PDFs: pasted vector artwork in
//! documents and both sides of the acceptance comparison.

use pdfium_render::prelude::*;
use std::path::PathBuf;
use std::sync::OnceLock;

/// One process-wide binding: PDFium is not thread-safe, so pdfium-render's `sync` feature
/// serialises every call behind its own lock.
static PDFIUM: OnceLock<Option<Pdfium>> = OnceLock::new();

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

/// The thread's PDFium binding, or `None` when the library is not found.
pub fn get() -> Option<&'static Pdfium> {
    PDFIUM.get_or_init(bind).as_ref()
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
