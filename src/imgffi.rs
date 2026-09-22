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

#[repr(C)]
#[derive(Clone, Copy)]
struct GInk {
    page: c_int,
    paths: c_int,
    glyphs: c_int,
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
    fn glean_ink_data(g: *mut GImages) -> *const GInk;
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
    /// No text and something drawn that could hold words: a page-sized raster,
    /// or vector ink of any size. This is the one that needs OCR.
    Scan,
    /// Small rasters, no text, no ink: a plate of photographs. Nothing on it
    /// can be read by OCR either.
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

/// One page's vector ink, as the survey counts it: every stroke and fill, and
/// the glyph-sized subset (see `GLYPH_INK_MIN`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ink {
    pub page: usize,
    pub paths: usize,
    pub glyphs: usize,
}

/// The ink on one page, if the survey saw any.
pub fn ink_on(page: usize, ink: &[Ink]) -> Option<&Ink> {
    ink.iter().find(|k| k.page == page)
}

/// A raster covering this much of the page is the page, not a picture on it.
/// Deliberately well below 1.0: a scan is inset by its margins, and 235 Carlaw's
/// scanned appendices land at 0.86–0.94 of the page box.
pub const SCAN_COVERAGE: f64 = 0.5;

/// This many glyph-sized fills on a page is text drawn as curves — a PDF whose
/// fonts were outlined at print time, which is how a "scanned" form arrives
/// with typed answers as the only real fonts (5775 Rue Ferrier's estoppel:
/// 80,000 path operators over three pages, two fonts, 31 words). A table draws
/// its rules as a few dozen long paths and a chart its bars as a few hundred
/// wide ones; neither reaches a hundred letter-sized fills. A line of outlined
/// 12pt type is about eighty. So a hundred is roughly "more than one line of
/// text that only OCR can read", and the cost of being wrong about a page is
/// one page of OCR.
pub const GLYPH_INK_MIN: usize = 100;

/// A page that holds this many words or fewer, after the running chrome and
/// the margin furniture are set aside, is SPARSE: not thin enough to be judged
/// by `classify_thin`'s liberal rule, but nowhere near a page of prose (300–600
/// words on a letter sheet). A form filled in by typewriter carries tens of
/// words over printed labels; whether those labels are readable is what the
/// survey then asks. Sixty is set between the fullest typed-overlay page on
/// file (31 words, the Ferrier Exhibit A) and the sparsest page that is
/// genuinely text (an operating statement with a small table: ~50 words over
/// a few dozen rules — which the glyph bar, not the word count, keeps out).
pub const SPARSE_WORDS_MAX: usize = 60;

/// Initialise poppler's process-wide state before any worker thread exists.
/// See the note on `glean_init` in images.cpp — poppler-cpp races its own
/// lazy `globalParams` construction across threads, and this is what closes it.
/// Idempotent; call it once from main.
pub fn init() {
    unsafe { glean_init() };
}

/// Where every raster sits, and which pages carry path ink — without decoding a
/// pixel or writing a file. Used to classify the pages that produced no words.
pub fn probe(pdf: &str, first: usize, last: usize) -> Result<(Vec<Image>, Vec<Ink>), String> {
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
        ink.extend(data.iter().map(|k| Ink {
            page: k.page as usize,
            paths: k.paths.max(0) as usize,
            glyphs: k.glyphs.max(0) as usize,
        }));
    }
    unsafe { glean_images_free(g) };
    Ok((imgs, ink))
}

/// Classify one wordless page from the survey. `page` is 1-based.
///
/// OUTLINED TEXT ON A WORDLESS PAGE IS A SCAN. This used to file every kind of
/// ink as `Image` ("a figure or a chart"), which no caller sends to OCR. The
/// 5775 Rue Ferrier estoppel (2026-09-21) is the cost of that: a scanned
/// certificate whose "scan" is 80,000 path operators — the form outlined at
/// print time — with the tenant's typed answers as the only fonts. Page 1, the
/// certificate body, has no words and no raster, was called `image`, and was
/// never read; page 3 carried the typed answers over the outlined labels, and
/// the reader saw a security deposit with nothing beside it and called it the
/// rent. The tell is the SHAPE of the ink, not its presence: thousands of
/// letter-sized fills are letters. A chart's bars and a drawing's lines stay
/// `Image`, as does the plate of small photographs — nothing on those reads.
pub fn classify(page: usize, w: f64, h: f64, imgs: &[Image], ink: &[Ink]) -> PageKind {
    let mine: Vec<&Image> = imgs.iter().filter(|i| i.page == page).collect();
    if mine.iter().any(|i| i.page_fraction(w, h) >= SCAN_COVERAGE) {
        return PageKind::Scan;
    }
    let k = ink_on(page, ink);
    if k.is_some_and(|k| k.glyphs >= GLYPH_INK_MIN) {
        return PageKind::Scan;
    }
    if !mine.is_empty() || k.is_some() {
        return PageKind::Image;
    }
    PageKind::Blank
}

/// A SPARSE page — more words than `classify_thin` will look at, but no more
/// than `SPARSE_WORDS_MAX` — is promoted to Scan only on the two signals that
/// mean "the page is a picture with words typed on it": a page-sized raster,
/// or outlined text in quantity. It is never promoted on ink alone: at this
/// word count the ink is usually a small table's rules, and a page whose exact
/// figures are in the text layer must not be traded for an OCR engine's
/// reading of them. Never demoted either — the words it holds are kept.
pub fn classify_sparse(page: usize, w: f64, h: f64, imgs: &[Image], ink: &[Ink]) -> PageKind {
    let raster = imgs
        .iter()
        .filter(|i| i.page == page)
        .any(|i| i.page_fraction(w, h) >= SCAN_COVERAGE);
    if raster || ink_on(page, ink).is_some_and(|k| k.glyphs >= GLYPH_INK_MIN) {
        PageKind::Scan
    } else {
        PageKind::Text
    }
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
/// VECTOR INK COUNTS, and assuming otherwise cost 8 of 9 misses on the first
/// unseen document set. The original reasoning — "a chart drawn with path
/// operators carries its labels in the text layer, so such a page is not
/// text-free and never reaches here" — is simply false for a PDF whose text has
/// been converted to outlines. An appraisal exported that way has pages
/// carrying 51 characters of running head, NO raster at all, and 87–145 words
/// that only OCR can see. Nothing about the page is a picture; the letters are
/// curves.
///
/// So the rule is: on a page with no text of its own, anything drawn on it is
/// its content. Rasters have to clear a size bar because a plate of small photos
/// is a plate of photos; ink does not, because ink with no text beside it has no
/// benign reading. A page with neither stays Text — a title over nothing is not
/// a hole.
pub fn classify_thin(page: usize, w: f64, h: f64, imgs: &[Image], ink: &[Ink]) -> PageKind {
    let cover: f64 = imgs
        .iter()
        .filter(|i| i.page == page)
        .filter(|i| i.page_fraction(w, h) >= THIN_FIGURE_MIN)
        .map(|i| i.page_fraction(w, h))
        .sum();
    if cover.min(1.0) >= THIN_IMAGE_COVERAGE || ink_on(page, ink).is_some() {
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
