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
  -j, --jobs <n>        worker threads (default: available parallelism)
      --json            emit {pages:[{page,anchor,markdown}], full_markdown}
      --images <dir>    extract embedded images to <dir> as PNG
      --min-image <px>  smallest image side to keep (default 64)
      --figures         also rasterise vector-drawn charts (needs --images)
      --keep-chrome     keep running heads/footers (dropped by default)
      --front-matter    prepend a YAML block naming the source and any
                        pages that could not be read
      --page-marks      mark page boundaries as <!-- page N --> so a
                        citation can name the page it came from
      --stats           print page/word/table counts to stderr
  -h, --help            print this help
  -V, --version         print the version

Pages with no text layer are skipped and reported by --stats; glean does not
OCR. Exit codes: 0 ok, 1 read/convert error, 2 usage error.
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
        jobs: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
        stats: false,
        keep_chrome: false,
        front_matter: false,
        page_marks: false,
        json: false,
        images: None,
        min_image: 64,
        figures: false,
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
            "--images" => a.images = Some(it.next().ok_or("--images needs a directory")?),
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
                        Ok(d) => ch
                            .into_iter()
                            .map(|(slot, p)| {
                                let (pw, ph) = d.page_size(p);
                                (slot, layout::lines(d.words(p)), pw, ph)
                            })
                            .collect::<Vec<_>>(),
                        Err(_) => Vec::new(),
                    })
                })
                .collect();
            hs.into_iter().filter_map(|h| h.join().ok()).flatten().collect::<Vec<_>>()
        });
        for (slot, lines, pw, ph) in results {
            pages[slot] = (lines, ph);
            widths[slot] = pw;
        }
    }

    // A single page has no "across pages" to compare against, and two is not
    // evidence either; below that threshold nothing is suppressed.
    let chrome = if args.keep_chrome || wanted.len() < 4 {
        std::collections::HashSet::new()
    } else {
        // A running head need not be on every page — a report's appendices often
        // carry a different one, or none. An eighth of the document is enough
        // evidence when combined with the margin and same-band requirements,
        // which body text essentially never satisfies.
        // Document-level body size: a per-page estimate is dragged around by
        // whatever that page happens to contain.
        let mut sizes: Vec<f64> = pages
            .iter()
            .flat_map(|(ls, _)| ls.iter().map(|l| l.size))
            .collect();
        let body = layout::median(&mut sizes);
        layout::running_chrome(&pages, (wanted.len() / 4).max(5), body)
    };

    // Phase 2: serialise, dropping the chrome.
    let mut out: Vec<String> = vec![String::new(); wanted.len()];
    let (mut empty, mut tables, mut words) = (0usize, 0usize, 0usize);
    for (slot, (lines, ph)) in pages.iter().enumerate() {
        let (t, s) = render_lines(lines, widths[slot], *ph, &chrome);
        words += s.0;
        tables += s.1;
        if s.0 == 0 {
            empty += 1;
        }
        out[slot] = t;
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

    let joined = if args.json {
        render_json(&args, &wanted, &out, &doc, &images)
    } else {
        let mut doc_md = String::new();
        if args.front_matter {
            // An LLM asked to extract fields cannot tell "this lease has no
            // rent schedule" from "the rent schedule was on a page that could
            // not be read". Say which it is, up front, in the context window.
            doc_md.push_str("---\nsource: ");
            doc_md.push_str(&args.input);
            doc_md.push_str(&format!("\npages: {}\n", wanted.len()));
            if empty > 0 {
                doc_md.push_str(&format!(
                    "unreadable_pages: {empty}\nwarning: {empty} page(s) have no text layer and are \
                     absent from this document; they require OCR\n"
                ));
            }
            doc_md.push_str("---\n\n");
        }
        let mut first = true;
        for (slot, text) in out.iter().enumerate() {
            let t = text.trim();
            if t.is_empty() {
                continue;
            }
            if !first {
                doc_md.push_str("\n\n");
            }
            first = false;
            if args.page_marks {
                doc_md.push_str(&format!("<!-- page {} -->\n\n", wanted[slot] + 1));
            }
            doc_md.push_str(t);
        }
        doc_md
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
            "glean: {} pages, {words} words, {tables} tables, {empty} page(s) with no text layer",
            wanted.len()
        );
    }
    // Silence is the failure mode that bit anydoc on the ESA: say so loudly.
    if empty > 0 {
        eprintln!(
            "glean: warning: {empty} of {} page(s) have no text layer and were skipped; \
             they need OCR",
            wanted.len()
        );
    }
    if empty == wanted.len() {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Serialise one page's lines, dropping any that were identified as running
/// chrome. Returns the markdown plus (word count, table count).
fn render_lines(
    lines: &[layout::Line],
    page_w: f64,
    page_h: f64,
    chrome: &std::collections::HashSet<String>,
) -> (String, (usize, usize)) {
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
        return (String::new(), (0, 0));
    }
    let body = layout::body_size(&kept);

    let mut s = String::new();
    let mut ntab = 0;
    for col in layout::columns(&kept, page_w.max(1.0)) {
        let sub: Vec<layout::Line> = col.into_iter().map(|i| kept[i].clone()).collect();
        for b in layout::blocks(&sub, body, page_h.max(1.0)) {
            if matches!(b, layout::Block::Table(_)) {
                ntab += 1;
            }
            md::emit(&mut s, &b);
        }
    }
    (s, (n, ntab))
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
    out: &[String],
    doc: &ffi::Doc,
    images: &[imgffi::Image],
) -> String {
    let mut s = String::from("{\n  \"pages\": [\n");
    for (slot, &p) in wanted.iter().enumerate() {
        let md = out[slot].trim();
        let (w, h) = doc.page_size(p);
        let imgs: Vec<&imgffi::Image> = images.iter().filter(|i| i.page == p + 1).collect();
        s.push_str(&format!(
            "    {{\"page\": {}, \"anchor\": \"page-{}\", \"width\": {:.1}, \"height\": {:.1}, \"has_text\": {}, \"markdown\": \"{}\"",
            p + 1, p + 1, w, h, !md.is_empty(), jesc(md)
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
        s.push('}');
        if slot + 1 < wanted.len() {
            s.push(',');
        }
        s.push('\n');
    }
    let full: Vec<&str> = out.iter().map(|x| x.trim()).filter(|x| !x.is_empty()).collect();
    s.push_str(&format!(
        "  ],\n  \"full_markdown\": \"{}\",\n  \"meta\": {{\"source\": \"{}\", \"pages\": {}, \"images\": {}}}\n}}",
        jesc(&full.join("\n\n")),
        jesc(&args.input),
        wanted.len(),
        images.len()
    ));
    s
}
