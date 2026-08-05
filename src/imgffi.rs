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

/// Extract every embedded image at least `min_px` on a side into `outdir`.
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
