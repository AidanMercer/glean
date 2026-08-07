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
    fn glean_init();
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

/// Initialise poppler's process-wide state before any worker thread exists.
/// See the note on `glean_init` in images.cpp — poppler-cpp races its own
/// lazy `globalParams` construction across threads, and this is what closes it.
/// Idempotent; call it once from main.
pub fn init() {
    unsafe { glean_init() };
}

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

/// How much of a text-free page must be picture before the picture is the page.
/// A quarter clears a corner logo and a masthead banner and nothing else.
pub const THIN_IMAGE_COVERAGE: f64 = 0.25;

/// An image smaller than this is an illustration ON a page, never the content OF
/// one. Six photographs tiled at 7% apiece sum past any coverage bar while being
/// exactly the plate of site photos that must NOT be sent to OCR; two stacked
/// charts at 23% and 15% are a page of data that must be. Summing without a
/// floor cannot tell those apart, and the floor is what does.
///
/// Set between the two observed cases and nearer the SMALLER one: the cost of
/// admitting a photo plate is one page of OCR, and the cost of excluding a chart
/// is the chart. 0.07 (plate) and 0.15 (chart) are the measured neighbours.
pub const THIN_FIGURE_MIN: f64 = 0.10;

// ⚠ A "REPEATED GRAPHIC IS A TEMPLATE" RULE WAS TRIED HERE AND REVERTED ON
// EVIDENCE. Do not re-add it keyed on dimensions.
//
// The idea is sound and the target is real: an appraisal carries a
// section-divider illustration at 67% of the page on five pages, which clears
// every coverage bar and is worth nothing to OCR. Every genuine content image in
// that document (a location map, four photocopied zoning tables, two charts)
// appears exactly once, so "appears on 3+ pages ⇒ furniture" separated them
// perfectly on the corpus it was written against.
//
// It also silently DELETED 462 scanned pages across 100 documents of the wider
// corpus, because the probe has no pixel identity — only dimensions — and the
// pages of a scanned document all share dimensions. A DocuSign-stamped scanned
// lease is a text layer of envelope ids over N full-page rasters of identical
// size, which is indistinguishable from N placements of one graphic. The rule
// read a 35-page scanned Certificate of Corporate Authority as decoration.
//
// It cannot be rescued by thresholds: the divider (0.67) sits inside the
// coverage range real page scans occupy, so no size band separates them. The
// only correct version needs the probe to carry a content hash, which is an FFI
// change (glean_images_probe would have to digest each image's bytes). Until
// then the false positives it would have removed cost about $0.05 per 500 pages,
// and the false negatives it created cost a rent roll.

/// The kind of a page whose only text is running furniture.
///
/// On an ordinary page a figure is a figure: the text is the content and the
/// picture illustrates it, so `classify` reasonably reserves `Scan` for a raster
/// that covers the sheet. On a page with no text but a header that reasoning
/// inverts — the figure IS the content, and whether it covers 59% (a zoning
/// bylaw photocopied into an appraisal) or 35% (a location map with the
/// demographics printed inside it) makes no difference to the only question
/// worth asking: can anything read it without OCR.
///
/// Coverage is summed across the page's images and capped, so overlapping
/// figures double-count. That errs toward calling a page pictorial, which is the
/// safe direction: the cost of being wrong is one page of OCR, and the cost of
/// the opposite is a table nobody knows is missing.
///
/// Vector ink is deliberately not consulted. A chart drawn with path operators
/// carries its labels in the text layer, so a page holding one is not text-free
/// in the first place and never reaches here.
pub fn classify_thin(page: usize, w: f64, h: f64, imgs: &[Image]) -> PageKind {
    let cover: f64 = imgs
        .iter()
        .filter(|i| i.page == page)
        .filter(|i| i.page_fraction(w, h) >= THIN_FIGURE_MIN)
        .map(|i| i.page_fraction(w, h))
        .sum();
    if cover.min(1.0) >= THIN_IMAGE_COVERAGE {
        PageKind::Scan
    } else {
        PageKind::Text
    }
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
