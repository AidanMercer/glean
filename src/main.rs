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
    json: bool,
    images: Option<String>,
    min_image: u32,
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
        json: false,
        images: None,
        min_image: 64,
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
            "--json" => a.json = true,
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
    let mut out: Vec<String> = vec![String::new(); wanted.len()];
    let (mut empty, mut tables, mut words) = (0usize, 0usize, 0usize);

    if nthreads <= 1 {
        for (slot, &p) in wanted.iter().enumerate() {
            let (t, s) = render_page(&doc, p);
            words += s.0;
            tables += s.1;
            if s.0 == 0 {
                empty += 1;
            }
            out[slot] = t;
        }
    } else {
        // Pages are independent, so fan them out and reassemble in order. Each
        // worker opens its own handle: poppler's document is not thread-safe.
        let mut chunks: Vec<Vec<(usize, usize)>> = vec![Vec::new(); nthreads];
        for (slot, &p) in wanted.iter().enumerate() {
            chunks[slot % nthreads].push((slot, p));
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
                                let (t, s) = render_page(&d, p);
                                (slot, t, s)
                            })
                            .collect::<Vec<_>>(),
                        Err(_) => Vec::new(),
                    })
                })
                .collect();
            hs.into_iter().filter_map(|h| h.join().ok()).flatten().collect::<Vec<_>>()
        });
        for (slot, text, s) in results {
            words += s.0;
            tables += s.1;
            if s.0 == 0 {
                empty += 1;
            }
            out[slot] = text;
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
        match imgffi::extract(&args.input, dir, first, last, args.min_image) {
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
        out.iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
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

/// Returns the page's markdown plus (word count, table count).
fn render_page(doc: &ffi::Doc, page: usize) -> (String, (usize, usize)) {
    let words = doc.words(page);
    let n = words.len();
    if n == 0 {
        return (String::new(), (0, 0));
    }
    let (pw, ph) = doc.page_size(page);
    let lines = layout::lines(words);
    if std::env::var_os("GLEAN_DEBUG_LINES").is_some() {
        for l in &lines {
            let gaps: Vec<String> = l.words.windows(2)
                .map(|w| format!("{:.1}", w[1].x0 - w[0].x1)).collect();
            eprintln!("[line y={:6.1} size={:4.1} n={} gaps=[{}] em3={:.1}] {}",
                l.y, l.size, l.words.len(), gaps.join(","), l.size * 3.0,
                l.text().chars().take(70).collect::<String>());
        }
    }
    let body = layout::body_size(&lines);

    let mut s = String::new();
    let mut ntab = 0;
    for col in layout::columns(&lines, pw.max(1.0)) {
        let sub: Vec<layout::Line> = col.into_iter().map(|i| lines[i].clone()).collect();
        for b in layout::blocks(&sub, body, ph.max(1.0)) {
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
