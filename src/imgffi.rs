//! Bindings to the embedded-image extractor.

use std::ffi::{c_char, c_double, c_int, CStr, CString};

#[repr(C)]
#[derive(Clone, Copy)]
struct GImage {
    page: c_int,
    x0: c_double,
    y0: c_double,
    x1: c_double,
    y1: c_double,
    w: c_int,
    h: c_int,
    path: *const c_char,
}

#[allow(non_camel_case_types)]
enum GImages {}

extern "C" {
    fn glean_images(
        pdf: *const c_char,
        outdir: *const c_char,
        first: c_int,
        last: c_int,
        min_px: c_int,
    ) -> *mut GImages;
    fn glean_images_count(g: *mut GImages) -> c_int;
    fn glean_images_data(g: *mut GImages) -> *const GImage;
    fn glean_images_free(g: *mut GImages);
    fn glean_images_probe(pdf: *const c_char, first: c_int, last: c_int, min_px: c_int)
        -> *mut GImages;
    fn glean_ink_count(g: *mut GImages) -> c_int;
    fn glean_ink_data(g: *mut GImages) -> *const c_int;
    fn glean_render_pages(
        pdf: *const c_char,
        outdir: *const c_char,
        pages: *const c_int,
        n: c_int,
        dpis: *const c_double,
    ) -> c_int;
    fn glean_figures(
        pdf: *const c_char,
        outdir: *const c_char,
        first: c_int,
        last: c_int,
        min_side_pts: c_double,
        dpi: c_double,
        into: *mut GImages,
    ) -> c_int;
}

#[derive(Debug, Clone)]
pub struct Image {
    pub page: usize,
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    pub w: u32,
    pub h: u32,
    pub path: String,
}

impl Image {
    /// Fraction of the page this image covers. A value near 1.0 means the
    /// "image" is a scan backdrop, not a photograph — the caller usually wants
    /// to drop those rather than treat them as figures.
    pub fn page_fraction(&self, page_w: f64, page_h: f64) -> f64 {
        if page_w <= 0.0 || page_h <= 0.0 {
            return 0.0;
        }
        (((self.x1 - self.x0) * (self.y1 - self.y0)) / (page_w * page_h)).abs()
    }
}

/// What a page with no text on it actually is.
///
/// `--front-matter` tells a model how many pages it is not being shown, and that
/// number is only worth having if it is true. A blank separator sheet is not a
/// missing page; a figure is not a missing page either, only a missing figure.
/// Reporting all three as "needs OCR" over-counts the hole and sends a caller to
/// pay for OCR on paper that has nothing on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    /// The page has a text layer and was read.
    Text,
    /// A page-sized raster with no text: a scan. This is the one that needs OCR.
    Scan,
    /// Rasters or vector ink, but nothing page-sized: a figure or a chart.
    Image,
    /// No text, no ink, no rasters. Nothing is missing.
    Blank,
}

impl PageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PageKind::Text => "text",
            PageKind::Scan => "scan",
            PageKind::Image => "image",
            PageKind::Blank => "blank",
        }
    }
}

/// A raster covering this much of the page is the page, not a picture on it.
/// Deliberately well below 1.0: a scan is inset by its margins, and 235 Carlaw's
/// scanned appendices land at 0.86–0.94 of the page box.
pub const SCAN_COVERAGE: f64 = 0.5;

/// Where every raster sits, and which pages carry path ink — without decoding a
/// pixel or writing a file. Used to classify the pages that produced no words.
pub fn probe(pdf: &str, first: usize, last: usize) -> Result<(Vec<Image>, Vec<usize>), String> {
    let p = CString::new(pdf).map_err(|e| e.to_string())?;
    // 16px, not the --images default of 64: this is asking "is there anything
    // here", not "is this worth keeping", and a low-resolution fax scan is still
    // a scan.
    let g = unsafe { glean_images_probe(p.as_ptr(), first as c_int, last as c_int, 16) };
    if g.is_null() {
        return Err(format!("could not open {pdf} to survey its pages"));
    }
    let n = unsafe { glean_images_count(g) } as usize;
    let mut imgs = Vec::with_capacity(n);
    if n > 0 {
        let data = unsafe { std::slice::from_raw_parts(glean_images_data(g), n) };
        for i in data {
            // `path` is null in probe mode — nothing was written. Never read it.
            imgs.push(Image {
                page: i.page as usize,
                x0: i.x0,
                y0: i.y0,
                x1: i.x1,
                y1: i.y1,
                w: i.w as u32,
                h: i.h as u32,
                path: String::new(),
            });
        }
    }
    let m = unsafe { glean_ink_count(g) } as usize;
    let mut ink = Vec::with_capacity(m);
    if m > 0 {
        let data = unsafe { std::slice::from_raw_parts(glean_ink_data(g), m) };
        ink.extend(data.iter().map(|&p| p as usize));
    }
    unsafe { glean_images_free(g) };
    Ok((imgs, ink))
}

/// Classify one wordless page from the survey. `page` is 1-based.
pub fn classify(page: usize, w: f64, h: f64, imgs: &[Image], ink: &[usize]) -> PageKind {
    let mine: Vec<&Image> = imgs.iter().filter(|i| i.page == page).collect();
    if mine.iter().any(|i| i.page_fraction(w, h) >= SCAN_COVERAGE) {
        return PageKind::Scan;
    }
    if !mine.is_empty() || ink.contains(&page) {
        return PageKind::Image;
    }
    PageKind::Blank
}

/// Bounds on the resolution a scan is re-rendered at. Below the floor OCR loses
/// small type; above the ceiling the file grows without carrying more ink than
/// the scan ever held.
pub const OCR_DPI_MIN: f64 = 150.0;
pub const OCR_DPI_MAX: f64 = 400.0;
pub const OCR_DPI_FALLBACK: f64 = 200.0;

/// The resolution the scan on this page was actually captured at, derived from
/// its largest raster: pixels across, over the width in inches it is drawn at.
/// Re-rendering at the source's own resolution neither discards detail nor
/// fabricates it.
pub fn native_dpi(page: usize, imgs: &[Image]) -> f64 {
    let widest = imgs
        .iter()
        .filter(|i| i.page == page && (i.x1 - i.x0).abs() > 1.0)
        .max_by(|a, b| (a.w).cmp(&b.w));
    match widest {
        Some(i) => {
            let inches = (i.x1 - i.x0).abs() / 72.0;
            let dpi = f64::from(i.w) / inches;
            dpi.clamp(OCR_DPI_MIN, OCR_DPI_MAX)
        }
        None => OCR_DPI_FALLBACK,
    }
}

/// Write one PNG per page, at the given per-page resolution. Returns how many
/// were written.
pub fn render_pages(pdf: &str, outdir: &str, pages: &[usize], dpis: &[f64]) -> Result<usize, String> {
    if pages.is_empty() {
        return Ok(0);
    }
    let p = CString::new(pdf).map_err(|e| e.to_string())?;
    let d = CString::new(outdir).map_err(|e| e.to_string())?;
    let ps: Vec<c_int> = pages.iter().map(|&n| n as c_int).collect();
    let n = unsafe {
        glean_render_pages(p.as_ptr(), d.as_ptr(), ps.as_ptr(), ps.len() as c_int, dpis.as_ptr())
    };
    if n < 0 {
        return Err(format!("could not render pages of {pdf}"));
    }
    Ok(n as usize)
}

/// Extract embedded rasters, and optionally rasterise vector-drawn figures too.
///
/// A chart built from path operators is not an image and never reaches
/// `drawImage`, so it has to be found by where ink lands and then re-rendered.
pub fn extract_all(
    pdf: &str,
    outdir: &str,
    first: usize,
    last: usize,
    min_px: u32,
    figures: Option<(f64, f64)>,
) -> Result<Vec<Image>, String> {
    let p = CString::new(pdf).map_err(|e| e.to_string())?;
    let d = CString::new(outdir).map_err(|e| e.to_string())?;
    let g = unsafe { glean_images(p.as_ptr(), d.as_ptr(), first as c_int, last as c_int, min_px as c_int) };
    if g.is_null() {
        return Err(format!("could not open {pdf} for image extraction"));
    }
    if let Some((min_side, dpi)) = figures {
        unsafe {
            glean_figures(p.as_ptr(), d.as_ptr(), first as c_int, last as c_int, min_side, dpi, g)
        };
    }
    let n = unsafe { glean_images_count(g) } as usize;
    let mut out = Vec::with_capacity(n);
    if n > 0 {
        let data = unsafe { std::slice::from_raw_parts(glean_images_data(g), n) };
        for i in data {
            out.push(Image {
                page: i.page as usize,
                x0: i.x0,
                y0: i.y0,
                x1: i.x1,
                y1: i.y1,
                w: i.w as u32,
                h: i.h as u32,
                path: unsafe { CStr::from_ptr(i.path) }.to_string_lossy().into_owned(),
            });
        }
    }
    unsafe { glean_images_free(g) };
    Ok(out)
}

#[allow(dead_code)]
pub fn extract(pdf: &str, outdir: &str, first: usize, last: usize, min_px: u32) -> Result<Vec<Image>, String> {
    let p = CString::new(pdf).map_err(|e| e.to_string())?;
    let d = CString::new(outdir).map_err(|e| e.to_string())?;
    let g = unsafe { glean_images(p.as_ptr(), d.as_ptr(), first as c_int, last as c_int, min_px as c_int) };
    if g.is_null() {
        return Err(format!("could not open {pdf} for image extraction"));
    }
    let n = unsafe { glean_images_count(g) } as usize;
    let mut out = Vec::with_capacity(n);
    if n > 0 {
        let data = unsafe { std::slice::from_raw_parts(glean_images_data(g), n) };
        for i in data {
            out.push(Image {
                page: i.page as usize,
                x0: i.x0,
                y0: i.y0,
                x1: i.x1,
                y1: i.y1,
                w: i.w as u32,
                h: i.h as u32,
                path: unsafe { CStr::from_ptr(i.path) }.to_string_lossy().into_owned(),
            });
        }
    }
    unsafe { glean_images_free(g) };
    Ok(out)
}
