//! glean — PDF to GitHub-Flavored Markdown.
//!
//! Extraction runs through poppler (correct glyphs, encodings, CID fonts);
//! layout, table reconstruction and serialisation are Rust.

mod ffi;
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

    let joined = out
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

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
