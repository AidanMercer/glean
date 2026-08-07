//! glean — PDF to GitHub-Flavored Markdown.
//!
//! Extraction runs through poppler (correct glyphs, encodings, CID fonts);
//! layout, table reconstruction and serialisation are Rust.

mod ffi;
mod imgffi;
mod layout;
mod md;

use std::io::{BufWriter, Write};
use std::process::ExitCode;

const USAGE: &str = "\
glean: convert a PDF to GitHub-Flavored Markdown

Usage:
  glean <file.pdf> [options]

Options:
  -o, --output <path>   write Markdown to <path> instead of stdout
  -p, --pages <spec>    only these pages, 1-based (e.g. 3, 5-9, 2,7,11-14)
  -j, --jobs <n>        worker threads (default 1). ⚠ n>1 is FASTER BUT NOT
                        SAFE: poppler's process-wide state races across glean's
                        workers and crashes about 1 document in 300, reporting
                        it as \"could not open\". Run several glean processes
                        instead — that shares nothing.
      --json            emit {pages:[{page,anchor,kind,markdown}], full_markdown}
      --images <dir>    extract embedded images to <dir> as PNG
      --min-image <px>  smallest image side to keep (default 64)
      --figures         also rasterise vector-drawn charts (needs --images)
      --cells           with --json, give every table cell a page-anchored
                        ref and its box, so a value can cite the cell
      --ocr-pages <dir> write one PNG per scanned page, ready to send to an
                        OCR engine; rendered at each scan's own resolution
      --ocr-dpi <n>     render at this resolution instead (150-400)
      --keep-chrome     keep running heads/footers (dropped by default)
      --front-matter    prepend a YAML block naming the source, the document's
                        length, and every way this copy of it is incomplete
      --page-marks      mark page boundaries as <!-- page N --> so a
                        citation can name the page it came from
      --stats           print page/word/table counts to stderr
  -h, --help            print this help
  -V, --version         print the version

--front-matter and --page-marks apply to --json too, where they shape
full_markdown.

glean does not OCR. Pages with no text layer are classified and reported: only
a `scan` needs an OCR engine — a figure or a blank page does not. Exit codes:
0 ok, 1 read/convert error or nothing readable, 2 usage error.
";

struct Args {
    input: String,
    output: Option<String>,
    pages: Option<Vec<usize>>,
    jobs: usize,
    stats: bool,
    keep_chrome: bool,
    front_matter: bool,
    page_marks: bool,
    json: bool,
    images: Option<String>,
    min_image: u32,
    figures: bool,
    ocr_pages: Option<String>,
    ocr_dpi: Option<f64>,
    cells: bool,
}

/// What the page survey concluded. Carried as one value because every consumer
/// of it — the front matter, the JSON, the stats line, the exit code — needs the
/// whole picture, and splitting it up is how the front matter and the JSON came
/// to disagree about what a page was in the first place.
struct Survey {
    /// 1-based page numbers, in document order.
    scanned: Vec<usize>,
    image_only: Vec<usize>,
    blank: usize,
}

/// One emitted table's cells, addressed the way the reader sees them — after
/// padding, blank rows and blank edge columns are gone.
struct TableCells {
    /// This table's order on its page, 1-based.
    index: usize,
    nrow: usize,
    ncol: usize,
    /// row, column (both 1-based, row 1 is the header), text, box on the page.
    cells: Vec<(usize, usize, String, layout::Rect)>,
}

/// Everything held per page of the output, in `wanted` order, plus what the
/// survey concluded about the document as a whole.
struct Pages {
    markdown: Vec<String>,
    kinds: Vec<imgffi::PageKind>,
    cells: Vec<Vec<TableCells>>,
    survey: Survey,
}

fn parse_pages(spec: &str) -> Option<Vec<usize>> {
    let mut v = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (a.trim().parse::<usize>().ok()?, b.trim().parse::<usize>().ok()?);
                if a == 0 || b < a {
                    return None;
                }
                v.extend(a..=b);
            }
            None => {
                let n = part.parse::<usize>().ok()?;
                if n == 0 {
                    return None;
                }
                v.push(n);
            }
        }
    }
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    v.dedup();
    Some(v)
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        input: String::new(),
        output: None,
        pages: None,
        // ONE by default, and that is a correctness decision, not a conservative
        // guess. See the note on OPEN_LOCK in ffi.rs: poppler's process-wide
        // state is not safe against glean's own workers, and the failure is a
        // SIGSEGV that prints "could not open <file>" — indistinguishable from an
        // encrypted PDF, so a lost document looks like a bad document. Measured
        // at 1–4 losses per 2,248 conversions with threads, 0 in 4,496 without.
        //
        // The cost is small and bounded: on the largest document in the corpus
        // (1,114 pages) single-threaded takes 7.2s against 4.9s at -j 16, and
        // that document's scanned pages alone are minutes of OCR. Ordinary deal
        // documents are tens of pages, where the difference is milliseconds.
        // Throughput over a folder comes from running several glean PROCESSES,
        // which shares nothing and is safe.
        jobs: 1,
        stats: false,
        keep_chrome: false,
        front_matter: false,
        page_marks: false,
        json: false,
        images: None,
        min_image: 64,
        figures: false,
        ocr_pages: None,
        ocr_dpi: None,
        cells: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("glean {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--stats" => a.stats = true,
            "--keep-chrome" => a.keep_chrome = true,
            "--front-matter" => a.front_matter = true,
            "--page-marks" => a.page_marks = true,
            "--json" => a.json = true,
            "--figures" => a.figures = true,
            "--cells" => a.cells = true,
            "--images" => a.images = Some(it.next().ok_or("--images needs a directory")?),
            "--ocr-pages" => a.ocr_pages = Some(it.next().ok_or("--ocr-pages needs a directory")?),
            "--ocr-dpi" => {
                let s = it.next().ok_or("--ocr-dpi needs a number")?;
                let n: f64 = s.parse().map_err(|_| format!("bad dpi: {s}"))?;
                if !(imgffi::OCR_DPI_MIN..=imgffi::OCR_DPI_MAX).contains(&n) {
                    return Err(format!(
                        "--ocr-dpi must be between {} and {}",
                        imgffi::OCR_DPI_MIN, imgffi::OCR_DPI_MAX
                    ));
                }
                a.ocr_dpi = Some(n);
            }
            "--min-image" => {
                let s = it.next().ok_or("--min-image needs a number")?;
                a.min_image = s.parse().map_err(|_| format!("bad pixel size: {s}"))?;
            }
            "-o" | "--output" => a.output = Some(it.next().ok_or("-o needs a path")?),
            "-p" | "--pages" => {
                let s = it.next().ok_or("-p needs a page spec")?;
                a.pages = Some(parse_pages(&s).ok_or_else(|| format!("bad page spec: {s}"))?);
            }
            "-j" | "--jobs" => {
                let s = it.next().ok_or("-j needs a number")?;
                a.jobs = s.parse().map_err(|_| format!("bad thread count: {s}"))?;
                if a.jobs == 0 {
                    return Err("--jobs must be at least 1".into());
                }
            }
            _ if arg.starts_with('-') && arg.len() > 1 => {
                return Err(format!("unknown option: {arg}"))
            }
            _ if a.input.is_empty() => a.input = arg,
            _ => return Err("only one input file at a time".into()),
        }
    }
    if a.input.is_empty() {
        return Err("no input file".into());
    }
    Ok(a)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("glean: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    // Before ANY document is opened and long before the workers spawn. poppler
    // constructs its process-wide `globalParams` lazily and unlocked, so N
    // threads opening N documents on a cold process race to build it and free
    // each other's — a SIGSEGV in roughly 1 document in 300 at -P 12, and never
    // once running serially. Doing it here means every later check finds it set.
    imgffi::init();

    let doc = match ffi::Doc::open(&args.input) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("glean: {e}");
            return ExitCode::from(1);
        }
    };

    let wanted: Vec<usize> = match &args.pages {
        Some(p) => p.iter().filter(|&&n| n <= doc.pages).map(|n| n - 1).collect(),
        None => (0..doc.pages).collect(),
    };
    if wanted.is_empty() {
        eprintln!("glean: no such pages in a {}-page document", doc.pages);
        return ExitCode::from(2);
    }
    // Asking for pages past the end is not fatal — but it is not nothing either,
    // and clamping in silence is the failure this tool exists to not commit.
    if let Some(p) = &args.pages {
        let dropped = p.len() - wanted.len();
        if dropped > 0 {
            eprintln!(
                "glean: warning: {dropped} requested page(s) are past the end of a {}-page \
                 document and were ignored",
                doc.pages
            );
        }
    }

    let nthreads = args.jobs.min(wanted.len());

    // Phase 1: extract every page's lines. Serialisation is deferred because
    // running heads and footers can only be identified by looking across pages —
    // nothing about `Docusign Envelope ID: …` marks it as chrome until you see
    // it on all 59. Pages are independent here, so this fans out; each worker
    // opens its own handle, poppler's document not being thread-safe.
    let mut pages: Vec<(Vec<layout::Line>, f64)> = vec![(Vec::new(), 0.0); wanted.len()];
    let mut widths: Vec<f64> = vec![0.0; wanted.len()];
    {
        let mut chunks: Vec<Vec<(usize, usize)>> = vec![Vec::new(); nthreads.max(1)];
        for (slot, &p) in wanted.iter().enumerate() {
            chunks[slot % nthreads.max(1)].push((slot, p));
        }
        let path = args.input.as_str();
        let results = std::thread::scope(|sc| {
            let hs: Vec<_> = chunks
                .into_iter()
                .map(|ch| {
                    sc.spawn(move || match ffi::Doc::open(path) {
                        Ok(d) => Ok(ch
                            .into_iter()
                            .map(|(slot, p)| {
                                let (pw, ph) = d.page_size(p);
                                (slot, layout::lines(d.words(p)), pw, ph)
                            })
                            .collect::<Vec<_>>()),
                        Err(e) => Err(e),
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("worker panicked".into())))
                .collect::<Vec<_>>()
        });
        // A worker that could not reopen the document leaves its pages empty, and
        // an empty page is indistinguishable from a scan by the time anything
        // downstream sees it. Reporting a read failure as "42 pages need OCR"
        // sends the caller to buy OCR for a bug. Fail here instead.
        for r in &results {
            if let Err(e) = r {
                eprintln!("glean: {e}");
                return ExitCode::from(1);
            }
        }
        for (slot, lines, pw, ph) in results.into_iter().flatten().flatten() {
            pages[slot] = (lines, ph);
            widths[slot] = pw;
        }
    }

    // A single page has no "across pages" to compare against, and two is not
    // evidence either; below that threshold nothing is suppressed.
    //
    // Detected ALWAYS, suppressed only when asked. `--keep-chrome` is an output
    // choice, and a page's NATURE must not change with one: the thin-page test
    // below asks "is anything here but the running head", and if that question
    // answered differently under a print flag, the same document would bill
    // differently for OCR depending on how it was asked to render.
    // Document-level body size: a per-page estimate is dragged around by
    // whatever that page happens to contain. Needed by both the chrome set and
    // the furniture test below, so it is computed once for the document.
    let body = {
        let mut sizes: Vec<f64> = pages
            .iter()
            .flat_map(|(ls, _)| ls.iter().map(|l| l.size))
            .collect();
        layout::median(&mut sizes)
    };
    let chrome = if wanted.len() < 4 {
        std::collections::HashSet::new()
    } else {
        // A running head need not be on every page — a report's appendices often
        // carry a different one, or none. An eighth of the document is enough
        // evidence when combined with the margin and same-band requirements,
        // which body text essentially never satisfies.
        layout::running_chrome(&pages, (wanted.len() / 4).max(5), body)
    };
    let drop_chrome = if args.keep_chrome {
        std::collections::HashSet::new()
    } else {
        chrome.clone()
    };

    // Phase 2: serialise, dropping the chrome.
    let mut out: Vec<String> = vec![String::new(); wanted.len()];
    let mut kinds: Vec<imgffi::PageKind> = vec![imgffi::PageKind::Text; wanted.len()];
    let mut cells: Vec<Vec<TableCells>> = Vec::with_capacity(wanted.len());
    let (mut tables, mut words) = (0usize, 0usize);
    // A heading is a LABEL for content, not content. "Location Overview" over a
    // map with the demographics printed inside it leaves a page whose only real
    // text is two words, and holding out for strictly zero would keep every such
    // page — the standard CIM and appraisal furniture pages — out of the survey.
    //
    // The budget is a fraction of what a page of THIS document normally carries,
    // not a fixed word count: "a tenth of a typical page" means the same thing in
    // a dense lease and an airy CIM, where any constant is wrong in one of them.
    // A flat twelve words missed a CIM page headed with a lender line, a
    // borrower line and "Appendix B Rent Roll" — nineteen words of title over a
    // rent roll that exists only as a picture. The floor keeps the rule from
    // vanishing on a document of sparse pages.
    const THIN_WORDS_FLOOR: usize = 12;
    const THIN_SHARE: usize = 10;

    // A page whose whole text layer is the running head is READABLE but says
    // nothing — and what it says nothing about is usually a full-page image.
    // An appraisal reproduces the zoning bylaw as a scanned insert; the text
    // layer carries the running head and "Page 65", and
    // the bilingual table of provisions is pixels. Judged on `has_text` alone
    // that page is Text, is never sent to OCR, and the provisions are lost with
    // no warning — the worst failure available, because it is silent.
    //
    // So chrome-only pages are SURVEYED with the wordless ones. They are only
    // ever promoted (to Scan), never demoted to Blank: words we hold are words
    // we must not deny. Below the survey the promotion is narrowed further.
    let mut thin: Vec<usize> = Vec::new();
    let mut bare: Vec<usize> = vec![0; wanted.len()];
    for (slot, (lines, ph)) in pages.iter().enumerate() {
        let (t, s, tc) = render_lines(lines, widths[slot], *ph, &drop_chrome, args.cells);
        words += s.0;
        tables += s.1;
        out[slot] = t;
        cells.push(tc);
        // Judged BEFORE the chrome filter, deliberately. A page carrying nothing
        // but a running header HAS a text layer; we dropped it on purpose, and
        // billing the caller for OCR on our own suppression would be absurd.
        // Provisionally a scan — if the survey below cannot run, over-stating the
        // hole is the safe direction to be wrong in.
        let all: usize = lines.iter().map(|l| l.words.len()).sum();
        if all == 0 {
            kinds[slot] = imgffi::PageKind::Scan;
        }
        // Everything that is not the running head and not margin furniture.
        // Both are needed: the chrome set catches a repeated header, and
        // `is_furniture` catches the footer whose page number keeps it out of
        // that set.
        let own: Vec<&layout::Line> = lines
            .iter()
            .filter(|l| !chrome.contains(l.text().trim()) && !layout::is_furniture(l, *ph, body))
            .collect();
        bare[slot] = own.iter().map(|l| l.words.len()).sum();

    }

    // Typical page, measured over the pages that carry prose at all — including
    // the near-empty ones would drag the median toward zero on exactly the
    // documents this rule matters for.
    let budget = {
        let mut real: Vec<f64> = bare.iter().filter(|&&b| b > 0).map(|&b| b as f64).collect();
        let med = layout::median(&mut real) as usize;
        (med / THIN_SHARE).max(THIN_WORDS_FLOOR)
    };
    for (slot, b) in bare.iter().enumerate() {
        if kinds[slot] == imgffi::PageKind::Text && *b <= budget {
            thin.push(slot);
        }
    }

    // What each wordless page actually is. The survey costs one content-stream
    // walk with no pixel decoding, and only over the span that holds them — a
    // document with a text layer throughout never pays for it at all.
    let mut wordless: Vec<usize> =
        (0..wanted.len()).filter(|&s| kinds[s] != imgffi::PageKind::Text).collect();
    let thin_set: std::collections::HashSet<usize> = thin.iter().copied().collect();
    wordless.extend(thin.iter().copied());
    wordless.sort_unstable();
    let mut surveyed: Vec<imgffi::Image> = Vec::new();
    if !wordless.is_empty() {
        let lo = wanted[wordless[0]] + 1;
        let hi = wanted[*wordless.last().unwrap()] + 1;
        match imgffi::probe(&args.input, lo, hi) {
            Ok((imgs, ink)) => {
                for &slot in &wordless {
                    let (page, w, h) = (wanted[slot] + 1, widths[slot], pages[slot].1);
                    // A thin page is judged on its own terms (see classify_thin)
                    // and can only ever be PROMOTED to Scan. It is never called
                    // Blank or Image: this page holds words, and a survey must
                    // not deny text we are already carrying.
                    let k = if thin_set.contains(&slot) {
                        imgffi::classify_thin(page, w, h, &imgs, &ink)
                    } else {
                        imgffi::classify(page, w, h, &imgs, &ink)
                    };
                    if thin_set.contains(&slot) && k != imgffi::PageKind::Scan {
                        continue;
                    }
                    kinds[slot] = k;
                }
                surveyed = imgs;
            }
            Err(e) => eprintln!(
                "glean: warning: could not survey the wordless pages ({e}); \
                 reporting all of them as scans"
            ),
        }
    }

    let pages_of = |k: imgffi::PageKind| -> Vec<usize> {
        (0..wanted.len()).filter(|&s| kinds[s] == k).map(|s| wanted[s] + 1).collect()
    };
    let survey = Survey {
        scanned: pages_of(imgffi::PageKind::Scan),
        image_only: pages_of(imgffi::PageKind::Image),
        blank: pages_of(imgffi::PageKind::Blank).len(),
    };
    let page_data = Pages { markdown: out, kinds, cells, survey };
    let out = &page_data.markdown;
    let survey = &page_data.survey;

    // The reply to the warning, in the same run that raises it. Without this the
    // caller is told "pages 1-13, 15-19 need OCR" and then has to re-render
    // exactly those pages with another tool to act on it.
    if let Some(dir) = &args.ocr_pages {
        if survey.scanned.is_empty() {
            eprintln!("glean: no scanned pages to render");
        } else if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("glean: cannot create {dir}: {e}");
            return ExitCode::from(1);
        } else {
            let dpis: Vec<f64> = survey
                .scanned
                .iter()
                .map(|&p| args.ocr_dpi.unwrap_or_else(|| imgffi::native_dpi(p, &surveyed)))
                .collect();
            match imgffi::render_pages(&args.input, dir, &survey.scanned, &dpis) {
                Ok(n) => eprintln!(
                    "glean: wrote {n} page image(s) for OCR to {dir} ({}-{} dpi)",
                    dpis.iter().cloned().fold(f64::INFINITY, f64::min).round(),
                    dpis.iter().cloned().fold(0.0, f64::max).round()
                ),
                Err(e) => {
                    eprintln!("glean: {e}");
                    return ExitCode::from(1);
                }
            }
        }
    }

    // Images are extracted once for the whole document: the C++ side walks the
    // pages itself, and poppler's global params are not safe to race on.
    let mut images: Vec<imgffi::Image> = Vec::new();
    if let Some(dir) = &args.images {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("glean: cannot create {dir}: {e}");
            return ExitCode::from(1);
        }
        let first = wanted.first().map(|p| p + 1).unwrap_or(1);
        let last = wanted.last().map(|p| p + 1).unwrap_or(0);
        let figs = if args.figures { Some((72.0, 150.0)) } else { None };
        match imgffi::extract_all(&args.input, dir, first, last, args.min_image, figs) {
            Ok(v) => images = v,
            Err(e) => {
                eprintln!("glean: image extraction failed: {e}");
                return ExitCode::from(1);
            }
        }
    }

    // The document is assembled ONCE and both output modes serve it. --json
    // used to build its own `full_markdown` and so silently ignored both
    // --front-matter and --page-marks — the two flags that exist for the
    // field-extraction case, missing from the shape a field-extraction pipeline
    // actually reads. One string, no second implementation to drift.
    let mut document = String::new();
    if args.front_matter {
        document.push_str(&front_matter(&args, doc.pages, &wanted, survey));
    }
    let mut first = true;
    for (slot, text) in out.iter().enumerate() {
        let t = text.trim();
        if t.is_empty() {
            continue;
        }
        if !first {
            document.push_str("\n\n");
        }
        first = false;
        if args.page_marks {
            document.push_str(&format!("<!-- page {} -->\n\n", wanted[slot] + 1));
        }
        document.push_str(t);
    }

    let joined = if args.json {
        render_json(&args, &wanted, &page_data, &document, &doc, &images)
    } else {
        document.clone()
    };

    let res = match &args.output {
        Some(p) => std::fs::write(p, joined.as_bytes()).map_err(|e| e.to_string()),
        None => {
            let so = std::io::stdout();
            let mut w = BufWriter::new(so.lock());
            w.write_all(joined.as_bytes())
                .and_then(|_| w.write_all(b"\n"))
                .map_err(|e| e.to_string())
        }
    };
    if let Err(e) = res {
        eprintln!("glean: write failed: {e}");
        return ExitCode::from(1);
    }

    if args.stats {
        if !images.is_empty() {
            eprintln!("glean: extracted {} image(s) to {}", images.len(),
                args.images.as_deref().unwrap_or("."));
        }
        eprintln!(
            "glean: {} pages, {words} words, {tables} tables, {} scan(s), {} image-only, {} blank",
            wanted.len(), survey.scanned.len(), survey.image_only.len(), survey.blank
        );
    }
    // Silence is the failure mode that bit anydoc on the ESA: say so loudly.
    // Only the scans are named here — an image-only page is a missing figure, a
    // blank page is missing nothing, and rolling all three into one OCR bill was
    // this warning over-stating its own case.
    if !survey.scanned.is_empty() {
        // "no readable text", not "no text layer": a page carrying a running
        // head and a title over a full-page scan HAS a text layer, and this
        // warning naming it a scan has to be true of that page too.
        eprintln!(
            "glean: warning: {} of {} page(s) carry no readable text and were skipped; \
             they need OCR: {}",
            survey.scanned.len(), wanted.len(), fmt_ranges(&survey.scanned)
        );
    }
    if out.iter().all(|t| t.trim().is_empty()) {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Collapse a page list to ranges: `[3, 5, 6, 7, 40]` → `3, 5-7, 40`.
/// A scanned appendix is hundreds of consecutive pages and printing every number
/// would put more noise in the context window than the fact is worth.
fn fmt_ranges(v: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let start = v[i];
        let mut end = start;
        while i + 1 < v.len() && v[i + 1] == end + 1 {
            i += 1;
            end = v[i];
        }
        out.push(if start == end { start.to_string() } else { format!("{start}-{end}") });
        i += 1;
    }
    out.join(", ")
}

/// Quote a scalar for YAML. The front matter is machine-read, so a path with a
/// colon in it must not silently turn the block into a different mapping.
fn yq(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The block that tells a model what it is not being shown.
///
/// Two different holes can open in a document handed to an extractor, and a model
/// answers confidently over either: pages that could not be read, and pages that
/// were never asked for. `-p 2,4 --front-matter` used to report `pages: 2` — the
/// size of the SUBSET as the size of the DOCUMENT — which is the same lie the
/// flag was written to prevent, told about the other hole.
fn front_matter(args: &Args, doc_pages: usize, wanted: &[usize], sv: &Survey) -> String {
    let mut s = String::from("---\n");
    s.push_str(&format!("source: {}\n", yq(&args.input)));
    s.push_str(&format!("pages: {doc_pages}\n"));
    let mut warnings: Vec<String> = Vec::new();
    if wanted.len() != doc_pages {
        let list: Vec<usize> = wanted.iter().map(|p| p + 1).collect();
        s.push_str(&format!("pages_included: {}\n", wanted.len()));
        s.push_str(&format!("included: {}\n", yq(&fmt_ranges(&list))));
        warnings.push(format!(
            "{} of {doc_pages} pages are not in this document; only page(s) {} were converted",
            doc_pages - wanted.len(),
            fmt_ranges(&list)
        ));
    }
    if !sv.scanned.is_empty() {
        s.push_str(&format!("unreadable_pages: {}\n", sv.scanned.len()));
        s.push_str(&format!("scanned_pages: {}\n", yq(&fmt_ranges(&sv.scanned))));
        // A thin page contributes its title and nothing else, so "absent" would
        // overstate it — but the CONTENT is absent, which is what a model asked
        // to pull a fact off that page has to be told.
        warnings.push(format!(
            "{} page(s) carry no readable text and their content is absent from this \
             document; they require OCR: {}",
            sv.scanned.len(),
            fmt_ranges(&sv.scanned)
        ));
    }
    // Stated, but not warned about: a figure is a missing picture, not missing
    // text, and a blank page is missing nothing at all.
    if !sv.image_only.is_empty() {
        s.push_str(&format!("image_only_pages: {}\n", yq(&fmt_ranges(&sv.image_only))));
    }
    if sv.blank > 0 {
        s.push_str(&format!("blank_pages: {}\n", sv.blank));
    }
    if !warnings.is_empty() {
        s.push_str("warnings:\n");
        for w in &warnings {
            s.push_str(&format!("  - {}\n", yq(w)));
        }
    }
    s.push_str("---\n\n");
    s
}

/// Serialise one page's lines, dropping any that were identified as running
/// chrome. Returns the markdown plus (word count, table count).
fn render_lines(
    lines: &[layout::Line],
    page_w: f64,
    page_h: f64,
    chrome: &std::collections::HashSet<String>,
    cells: bool,
) -> (String, (usize, usize), Vec<TableCells>) {
    let kept: Vec<layout::Line> = if chrome.is_empty() {
        lines.to_vec()
    } else {
        lines
            .iter()
            .filter(|l| !chrome.contains(l.text().trim()))
            .cloned()
            .collect()
    };
    let n: usize = kept.iter().map(|l| l.words.len()).sum();
    if n == 0 {
        return (String::new(), (0, 0), Vec::new());
    }
    let body = layout::body_size(&kept);

    let mut s = String::new();
    let mut ntab = 0;
    let mut tables = Vec::new();
    for col in layout::columns(&kept, page_w.max(1.0)) {
        let sub: Vec<layout::Line> = col.into_iter().map(|i| kept[i].clone()).collect();
        for b in layout::blocks(&sub, body, page_h.max(1.0)) {
            if let layout::Block::Table(t) = &b {
                ntab += 1;
                // Provenance comes off the SAME normalisation the Markdown is
                // written from, so a cited row and column address the table the
                // reader is looking at. A degenerate block is emitted as prose
                // and has no cells to cite.
                if cells {
                    if let Some(norm) = md::normalise(t) {
                        if !norm.degenerate {
                            tables.push(collect_cells(tables.len() + 1, &norm));
                        }
                    }
                }
            }
            md::emit(&mut s, &b);
        }
    }
    (s, (n, ntab), tables)
}

/// Every cell that says something and knows where it said it. A blank cell has
/// no box and nothing to cite, so it is left out rather than emitted empty.
fn collect_cells(index: usize, n: &md::Norm) -> TableCells {
    let mut cells = Vec::new();
    for (ri, row) in n.rows.iter().enumerate() {
        for (ci, text) in row.iter().enumerate() {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            if let Some(Some(b)) = n.boxes.get(ri).map(|r| r.get(ci).copied().flatten()) {
                cells.push((ri + 1, ci + 1, text.to_string(), b));
            }
        }
    }
    TableCells { index, nrow: n.rows.len(), ncol: n.rows[0].len(), cells }
}

/// JSON escape, sufficient for the subset we emit.
fn jesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// Page-addressable output. The shape mirrors what a hosted OCR API returns —
/// `pages[]` with an anchor each, plus the concatenated document — so a pipeline
/// written against one can consume the other with no translation layer.
fn render_json(
    args: &Args,
    wanted: &[usize],
    pg: &Pages,
    document: &str,
    doc: &ffi::Doc,
    images: &[imgffi::Image],
) -> String {
    let sv = &pg.survey;
    let out = &pg.markdown;
    let mut s = String::from("{\n  \"pages\": [\n");
    for (slot, &p) in wanted.iter().enumerate() {
        let md = out[slot].trim();
        let (w, h) = doc.page_size(p);
        let imgs: Vec<&imgffi::Image> = images.iter().filter(|i| i.page == p + 1).collect();
        // `has_text` reports the TEXT LAYER, not the output: a page whose every
        // line was running chrome had text and does not need OCR. `kind` is the
        // field to route on — only "scan" is worth paying an OCR engine for.
        s.push_str(&format!(
            "    {{\"page\": {}, \"anchor\": \"page-{}\", \"width\": {:.1}, \"height\": {:.1}, \"has_text\": {}, \"kind\": \"{}\", \"markdown\": \"{}\"",
            p + 1, p + 1, w, h,
            pg.kinds[slot] == imgffi::PageKind::Text,
            pg.kinds[slot].as_str(),
            jesc(md)
        ));
        if !imgs.is_empty() {
            s.push_str(", \"images\": [");
            for (k, i) in imgs.iter().enumerate() {
                if k > 0 {
                    s.push_str(", ");
                }
                s.push_str(&format!(
                    "{{\"path\": \"{}\", \"width\": {}, \"height\": {}, \"bbox\": [{:.1}, {:.1}, {:.1}, {:.1}], \"page_fraction\": {:.3}}}",
                    jesc(&i.path), i.w, i.h, i.x0, i.y0, i.x1, i.y1, i.page_fraction(w, h)
                ));
            }
            s.push(']');
        }
        // Cell provenance. A PDF-sourced value can now cite where it was read
        // the way a spreadsheet-sourced one does, instead of citing a whole
        // page and leaving the reader to search it. Opt-in, because it is
        // several cells of JSON per value and no use to a consumer that only
        // wants the prose.
        if let Some(ts) = pg.cells.get(slot).filter(|t| !t.is_empty()) {
            s.push_str(", \"tables\": [");
            for (k, t) in ts.iter().enumerate() {
                if k > 0 {
                    s.push_str(", ");
                }
                s.push_str(&format!(
                    "{{\"table\": {}, \"rows\": {}, \"cols\": {}, \"cells\": [",
                    t.index, t.nrow, t.ncol
                ));
                for (m, (r, c, text, b)) in t.cells.iter().enumerate() {
                    if m > 0 {
                        s.push_str(", ");
                    }
                    s.push_str(&format!(
                        "{{\"ref\": \"p{}!t{}!r{r}c{c}\", \"row\": {r}, \"col\": {c}, \"text\": \"{}\", \"bbox\": [{:.1}, {:.1}, {:.1}, {:.1}]}}",
                        p + 1, t.index, jesc(text), b[0], b[1], b[2], b[3]
                    ));
                }
                s.push_str("]}");
            }
            s.push(']');
        }
        s.push('}');
        if slot + 1 < wanted.len() {
            s.push(',');
        }
        s.push('\n');
    }
    // `full_markdown` is the document assembled once for both output modes, so
    // it carries the front matter and the page marks when they were asked for.
    // `pages` is the DOCUMENT's length; `pages_included` is how much of it is in
    // here. Conflating the two is how a two-page slice reads as a two-page deal.
    s.push_str(&format!(
        "  ],\n  \"full_markdown\": \"{}\",\n  \"meta\": {{\"source\": \"{}\", \"pages\": {}, \"pages_included\": {}, \"unreadable_pages\": {}, \"scanned_pages\": [{}], \"blank_pages\": {}, \"images\": {}}}\n}}",
        jesc(document.trim()),
        jesc(&args.input),
        doc.pages,
        wanted.len(),
        sv.scanned.len(),
        sv.scanned.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "),
        sv.blank,
        images.len()
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imgffi::{classify, Image, PageKind};

    fn args(input: &str) -> Args {
        Args {
            input: input.into(),
            output: None,
            pages: None,
            jobs: 1,
            stats: false,
            keep_chrome: false,
            front_matter: true,
            page_marks: false,
            json: false,
            images: None,
            min_image: 64,
            figures: false,
            ocr_pages: None,
            ocr_dpi: None,
            cells: false,
        }
    }

    fn survey(scanned: Vec<usize>, image_only: Vec<usize>, blank: usize) -> Survey {
        Survey { scanned, image_only, blank }
    }

    fn img(page: usize, x0: f64, y0: f64, x1: f64, y1: f64) -> Image {
        Image { page, x0, y0, x1, y1, w: 100, h: 100, path: String::new() }
    }

    #[test]
    fn consecutive_pages_collapse_to_ranges() {
        // A scanned appendix is hundreds of pages; naming each one costs more
        // context than the fact is worth.
        assert_eq!(fmt_ranges(&[3]), "3");
        assert_eq!(fmt_ranges(&[3, 5, 6, 7, 40]), "3, 5-7, 40");
        assert_eq!(fmt_ranges(&[1, 2, 3]), "1-3");
        assert_eq!(fmt_ranges(&[]), "");
    }

    #[test]
    fn a_page_subset_states_the_documents_length_not_its_own() {
        // `-p 2,4 --front-matter` reported `pages: 2` on a 33-page document —
        // the flag that exists to stop a model answering over a hole, opening
        // one. A model told the deal is two pages long says so with confidence.
        let fm = front_matter(&args("deal.pdf"), 33, &[1, 3], &survey(vec![], vec![], 0));
        assert!(fm.contains("pages: 33\n"), "{fm}");
        assert!(fm.contains("pages_included: 2\n"), "{fm}");
        assert!(fm.contains("included: \"2, 4\""), "{fm}");
        assert!(fm.contains("31 of 33 pages are not in this document"), "{fm}");
    }

    #[test]
    fn a_whole_document_says_nothing_about_subsets() {
        let fm = front_matter(&args("deal.pdf"), 4, &[0, 1, 2, 3], &survey(vec![], vec![], 0));
        assert!(fm.contains("pages: 4\n"), "{fm}");
        assert!(!fm.contains("pages_included"), "{fm}");
        assert!(!fm.contains("warnings"), "{fm}");
    }

    #[test]
    fn only_scans_are_reported_as_needing_ocr() {
        // The site-visit report is four pages: one of text and three of
        // photographs. Calling the photographs "pages that require OCR" tells a
        // model text is missing that never existed, and sends the caller to buy
        // OCR for pictures of a building.
        let fm = front_matter(&args("visit.pdf"), 4, &[0, 1, 2, 3], &survey(vec![], vec![2, 3, 4], 0));
        assert!(fm.contains("image_only_pages: \"2-4\""), "{fm}");
        assert!(!fm.contains("unreadable_pages"), "{fm}");
        assert!(!fm.contains("warnings"), "{fm}");
    }

    #[test]
    fn a_path_with_a_colon_stays_one_yaml_field() {
        // The block is machine-read; an unquoted colon silently re-parses it.
        let fm = front_matter(&args("C:/deals/rent: roll.pdf"), 1, &[0], &survey(vec![], vec![], 0));
        assert!(fm.contains("source: \"C:/deals/rent: roll.pdf\""), "{fm}");
    }

    #[test]
    fn a_page_sized_raster_is_a_scan_and_a_small_one_is_not() {
        // Both are "a page with no words and an image on it". Only the first
        // has text on it that OCR would recover.
        let scan = img(1, 20.0, 20.0, 592.0, 772.0);          // full bleed, 612x792
        let figure = img(2, 100.0, 400.0, 500.0, 620.0);      // a map, 0.22 of the page
        assert_eq!(classify(1, 612.0, 792.0, std::slice::from_ref(&scan), &[]), PageKind::Scan);
        assert_eq!(classify(2, 612.0, 792.0, &[figure], &[]), PageKind::Image);
        // The same raster, asked about from a page it is not on.
        assert_eq!(classify(3, 612.0, 792.0, &[scan], &[]), PageKind::Blank);
    }

    #[test]
    fn a_figure_on_a_text_free_page_is_the_page() {
        // The 0.36-coverage map that is a FIGURE on a page of prose is the whole
        // content of a page whose only text is "Location Overview" and a running
        // head — a real one off an appraisal, demographics printed
        // inside it. classify() reserves Scan for a full-bleed raster and is
        // right to; classify_thin asks the other question.
        let map = img(1, 80.0, 300.0, 530.0, 690.0);
        assert_eq!(classify(1, 612.0, 792.0, std::slice::from_ref(&map), &[]), PageKind::Image);
        // A logo and a banner must not carry a page over the bar on their own.
        let logo = img(2, 0.0, 0.0, 40.0, 40.0);
        let banner = img(2, 0.0, 700.0, 300.0, 760.0);
        assert_eq!(imgffi::classify_thin(2, 612.0, 792.0, &[logo, banner], &[]), PageKind::Text);
        // Map plus that furniture clears it — the figure is what the page holds.
        let logo1 = img(1, 0.0, 0.0, 40.0, 40.0);
        assert_eq!(
            imgffi::classify_thin(1, 612.0, 792.0, &[map, logo1], &[]),
            PageKind::Scan
        );
    }

    #[test]
    fn a_plate_of_photographs_is_not_a_scan() {
        // Six site photos tiled at 7% each sum to 42% — past any coverage bar,
        // and exactly the page that must not be billed for OCR. Two stacked
        // charts at 23% and 15% sum to less and must be.
        let tile = |n: usize| Image {
            page: 1, x0: 0.0, y0: n as f64 * 60.0, x1: 160.0, y1: n as f64 * 60.0 + 130.0,
            w: 400, h: 300, path: String::new(),
        };
        let plate: Vec<Image> = (0..6).map(tile).collect();
        assert!(plate[0].page_fraction(612.0, 792.0) < imgffi::THIN_FIGURE_MIN);
        assert_eq!(imgffi::classify_thin(1, 612.0, 792.0, &plate, &[]), PageKind::Text);

        let chart = |y0: f64, y1: f64, w: u32| Image {
            page: 1, x0: 60.0, y0, x1: 552.0, y1, w, h: 800, path: String::new(),
        };
        let charts = vec![chart(400.0, 690.0, 900), chart(120.0, 310.0, 901)];
        assert_eq!(imgffi::classify_thin(1, 612.0, 792.0, &charts, &[]), PageKind::Scan);
    }

    #[test]
    fn a_thin_page_is_never_demoted_to_blank() {
        // A title over nothing at all holds words we already have. Promotion is
        // the only direction available, so this stays Text and its title is kept.
        assert_eq!(imgffi::classify_thin(1, 612.0, 792.0, &[], &[]), PageKind::Text);
    }

    #[test]
    fn a_scan_is_rendered_at_its_own_resolution() {
        // A 300 dpi scan re-rendered at 150 loses half the ink OCR has to read,
        // and at 600 it invents nothing and quadruples the bytes. The scan says
        // what it was captured at: 2552 px across 8.5 inches of page.
        let scan = Image { page: 1, x0: 0.0, y0: 0.0, x1: 612.0, y1: 792.0,
                           w: 2552, h: 3294, path: String::new() };
        assert_eq!(imgffi::native_dpi(1, std::slice::from_ref(&scan)).round(), 300.0);
        // Nothing known about the page: a sane default, not a guess dressed up.
        assert_eq!(imgffi::native_dpi(9, &[scan]), imgffi::OCR_DPI_FALLBACK);
    }

    #[test]
    fn a_thumbnail_does_not_drag_the_render_resolution_up() {
        // A 64px icon drawn 2pt wide computes to thousands of dpi. Clamped.
        let icon = Image { page: 1, x0: 0.0, y0: 0.0, x1: 2.0, y1: 2.0,
                           w: 64, h: 64, path: String::new() };
        assert_eq!(imgffi::native_dpi(1, &[icon]), imgffi::OCR_DPI_MAX);
    }

    #[test]
    fn vector_ink_is_not_a_blank_page() {
        // A chart drawn with path operators has no words and no raster. It is
        // not a scan, but it is not nothing either.
        assert_eq!(classify(7, 612.0, 792.0, &[], &[7]), PageKind::Image);
        assert_eq!(classify(7, 612.0, 792.0, &[], &[9]), PageKind::Blank);
    }
}
