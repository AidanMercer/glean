//! GFM serialisation.
//!
//! Two rules drive everything here, both learned from auditing other parsers
//! against real appraisals:
//!   1. Never emit a ragged table. A pipe table whose rows disagree on column
//!      count is not a table to any downstream consumer, it is noise.
//!   2. Never silently weld two cells together. Escaping and cell padding are
//!      cheap; a fused "$5,120,000480" that reads as a real number is not.

use crate::layout::{Block, Line, Table};

fn escape_cell(s: &str) -> String {
    s.replace('\\', "\\\\").replace('|', "\\|").replace('\n', " ")
}

fn write_table(out: &mut String, t: &Table) {
    let ncol = t.rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if ncol < 2 || t.rows.is_empty() {
        return;
    }
    let mut rows = t.rows.clone();
    for r in &mut rows {
        r.resize(ncol, String::new());
    }
    // Drop wholly empty rows and edge columns — artefacts of a wide margin or a
    // rule line, and pure noise in an LLM's context window.
    rows.retain(|r| r.iter().any(|c| !c.trim().is_empty()));
    if rows.is_empty() {
        return;
    }
    while rows[0].len() > 2 && rows.iter().all(|r| r[0].trim().is_empty()) {
        for r in &mut rows {
            r.remove(0);
        }
    }
    while rows[0].len() > 2 && rows.iter().all(|r| r.last().is_some_and(|c| c.trim().is_empty())) {
        for r in &mut rows {
            r.pop();
        }
    }
    let ncol = rows[0].len();

    let (head, body): (&[Vec<String>], &[Vec<String>]) = if t.header && rows.len() > 1 {
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
