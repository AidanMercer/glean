//! GFM serialisation.
//!
//! Two rules drive everything here, both learned from auditing other parsers
//! against real appraisals:
//!   1. Never emit a ragged table. A pipe table whose rows disagree on column
//!      count is not a table to any downstream consumer, it is noise.
//!   2. Never silently weld two cells together. Escaping and cell padding are
//!      cheap; a fused "$5,120,000480" that reads as a real number is not.

use crate::layout::{Block, Line, Rect, Table};

fn escape_cell(s: &str) -> String {
    s.replace('\\', "\\\\").replace('|', "\\|").replace('\n', " ")
}

/// A table in the shape it is actually emitted in: padded to a rectangle, with
/// blank rows and blank edge columns dropped.
///
/// Provenance is computed against THIS, never against the built table. A cell's
/// row and column only mean anything in the grid the reader is looking at, and
/// the two disagree by however many empty edge columns were trimmed — a citation
/// off by one column is a citation to the wrong value, which is worse than none.
pub struct Norm {
    pub rows: Vec<Vec<String>>,
    pub boxes: Vec<Vec<Option<Rect>>>,
    pub header: bool,
    /// Too thin to be a table: emitted as plain text, and worth no citations.
    pub degenerate: bool,
}

pub fn normalise(t: &Table) -> Option<Norm> {
    let ncol = t.rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if ncol < 2 || t.rows.is_empty() {
        return None;
    }
    let mut rows = t.rows.clone();
    let mut boxes = t.boxes.clone();
    boxes.resize(rows.len(), Vec::new());
    for (r, b) in rows.iter_mut().zip(boxes.iter_mut()) {
        r.resize(ncol, String::new());
        b.resize(ncol, None);
    }
    // Drop wholly empty rows and edge columns — artefacts of a wide margin or a
    // rule line, and pure noise in an LLM's context window.
    let keep: Vec<bool> =
        rows.iter().map(|r| r.iter().any(|c| !c.trim().is_empty())).collect();
    let mut i = 0;
    rows.retain(|_| { i += 1; keep[i - 1] });
    let mut i = 0;
    boxes.retain(|_| { i += 1; keep[i - 1] });
    if rows.is_empty() {
        return None;
    }
    while rows[0].len() > 2 && rows.iter().all(|r| r[0].trim().is_empty()) {
        for r in &mut rows {
            r.remove(0);
        }
        for b in &mut boxes {
            if !b.is_empty() {
                b.remove(0);
            }
        }
    }
    while rows[0].len() > 2 && rows.iter().all(|r| r.last().is_some_and(|c| c.trim().is_empty())) {
        for r in &mut rows {
            r.pop();
        }
        for b in &mut boxes {
            b.pop();
        }
    }

    // A table with no header text and barely any body is a layout accident — an
    // empty ruled box, or a stray pair of aligned words. Emitting `| | |` puts
    // pure noise in an LLM's context window.
    let substantive = rows
        .iter()
        .filter(|r| r.iter().filter(|c| !c.trim().is_empty()).count() >= 2)
        .count();
    let header_blank = rows[0].iter().all(|c| c.trim().is_empty());
    let degenerate = substantive < 2 || (header_blank && substantive < 3);
    Some(Norm { rows, boxes, header: t.header, degenerate })
}

fn write_table(out: &mut String, t: &Table) {
    let Some(n) = normalise(t) else { return };
    let rows = n.rows;
    if n.degenerate {
        for r in &rows {
            let line = r.iter().map(|c| c.trim()).filter(|c| !c.is_empty())
                .collect::<Vec<_>>().join(" ");
            if !line.is_empty() {
                out.push_str(&line);
                out.push_str("\n\n");
            }
        }
        return;
    }
    let ncol = rows[0].len();

    let (head, body): (&[Vec<String>], &[Vec<String>]) = if n.header && rows.len() > 1 {
        (&rows[..1], &rows[1..])
    } else {
        (&[], &rows[..])
    };

    out.push('\n');
    let blank: Vec<String> = vec![String::new(); ncol];
    let h = head.first().unwrap_or(&blank);
    out.push_str("| ");
    out.push_str(&h.iter().map(|c| escape_cell(c.trim())).collect::<Vec<_>>().join(" | "));
    out.push_str(" |\n|");
    for _ in 0..ncol {
        out.push_str(" --- |");
    }
    out.push('\n');
    for r in body {
        out.push_str("| ");
        out.push_str(&r.iter().map(|c| escape_cell(c.trim())).collect::<Vec<_>>().join(" | "));
        out.push_str(" |\n");
    }
    out.push('\n');
}

fn inline(l: &Line) -> String {
    let mut s = String::new();
    for (i, w) in l.words.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&w.text);
    }
    s
}

pub fn emit(out: &mut String, b: &Block) {
    match b {
        Block::Heading(lvl, l) => {
            let text = inline(l);
            if text.trim().is_empty() {
                return;
            }
            out.push('\n');
            for _ in 0..*lvl {
                out.push('#');
            }
            out.push(' ');
            out.push_str(text.trim());
            out.push_str("\n\n");
        }
        Block::Para(lines) => {
            // Rejoin a wrapped paragraph into one logical line; a hyphen at the
            // end of a line is a break, not part of the word.
            let mut s = String::new();
            for l in lines {
                let t = inline(l);
                let t = t.trim();
                if t.is_empty() {
                    continue;
                }
                if s.is_empty() {
                    s.push_str(t);
                } else if s.ends_with('-') && !s.ends_with(" -") {
                    s.pop();
                    s.push_str(t);
                } else {
                    s.push(' ');
                    s.push_str(t);
                }
            }
            if s.trim().is_empty() {
                return;
            }
            out.push_str(s.trim());
            out.push_str("\n\n");
        }
        Block::Table(t) => write_table(out, t),
    }
}
