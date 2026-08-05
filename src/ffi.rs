//! Thin bindings to the poppler-cpp extraction shim.

use std::ffi::{c_char, c_double, c_int, CString};

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GWord {
    pub x0: c_double,
    pub y0: c_double,
    pub x1: c_double,
    pub y1: c_double,
    pub fsize: c_double,
    pub off: u32,
    pub len: u32,
    pub flags: c_int,
}

#[allow(non_camel_case_types)]
pub enum GDoc {}

extern "C" {
    fn glean_open(path: *const c_char) -> *mut GDoc;
    fn glean_pages(g: *mut GDoc) -> c_int;
    fn glean_load_page(g: *mut GDoc, idx: c_int) -> c_int;
    fn glean_words(g: *mut GDoc, idx: c_int, count: *mut c_int) -> *const GWord;
    fn glean_arena(g: *mut GDoc, idx: c_int, len: *mut c_int) -> *const c_char;
    fn glean_page_size(g: *mut GDoc, idx: c_int, w: *mut c_double, h: *mut c_double);
    fn glean_release_page(g: *mut GDoc, idx: c_int);
    fn glean_close(g: *mut GDoc);
}

pub struct Doc {
    raw: *mut GDoc,
    pub pages: usize,
}

/// One word with its position on the page. Text is owned so the page buffers
/// can be released immediately after extraction.
#[derive(Clone, Debug)]
pub struct Word {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    pub size: f64,
    pub bold: bool,
    pub text: String,
}

impl Word {
    pub fn ymid(&self) -> f64 {
        (self.y0 + self.y1) * 0.5
    }
}

impl Doc {
    pub fn open(path: &str) -> Result<Doc, String> {
        let c = CString::new(path).map_err(|e| e.to_string())?;
        let raw = unsafe { glean_open(c.as_ptr()) };
        if raw.is_null() {
            return Err(format!("could not open {path} (encrypted or not a PDF)"));
        }
        let pages = unsafe { glean_pages(raw) } as usize;
        Ok(Doc { raw, pages })
    }

    pub fn page_size(&self, idx: usize) -> (f64, f64) {
        let (mut w, mut h) = (0.0, 0.0);
        unsafe { glean_page_size(self.raw, idx as c_int, &mut w, &mut h) };
        (w, h)
    }

    pub fn words(&self, idx: usize) -> Vec<Word> {
        let n = unsafe { glean_load_page(self.raw, idx as c_int) };
        if n <= 0 {
            return Vec::new();
        }
        let mut count = 0;
        let wp = unsafe { glean_words(self.raw, idx as c_int, &mut count) };
        let mut alen = 0;
        let ap = unsafe { glean_arena(self.raw, idx as c_int, &mut alen) };
        if wp.is_null() || ap.is_null() {
            return Vec::new();
        }
        let raws = unsafe { std::slice::from_raw_parts(wp, count as usize) };
        let arena = unsafe { std::slice::from_raw_parts(ap as *const u8, alen as usize) };

        let out = raws
            .iter()
            .map(|w| {
                let s = &arena[w.off as usize..(w.off + w.len) as usize];
                Word {
                    x0: w.x0,
                    y0: w.y0,
                    x1: w.x1,
                    y1: w.y1,
                    size: if w.fsize > 0.0 { w.fsize } else { w.y1 - w.y0 },
                    bold: w.flags & 1 != 0,
                    text: String::from_utf8_lossy(s).into_owned(),
                }
            })
            .collect();
        unsafe { glean_release_page(self.raw, idx as c_int) };
        out
    }
}

impl Drop for Doc {
    fn drop(&mut self) {
        unsafe { glean_close(self.raw) };
    }
}

unsafe impl Send for Doc {}
