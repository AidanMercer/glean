//! Words -> lines -> columns -> blocks -> tables.
//!
//! The whole design rests on one observation from benchmarking anydoc against
//! PyMuPDF4LLM on real appraisals: borderless financial tables have no drawing
//! operations to key off, so the only reliable signal is *vertical whitespace
//! corridors* — x ranges that no word crosses on any row of the block. Detecting
//! columns that way handles ruled and unruled tables identically, and copes with
//! empty cells, which x-anchor clustering does not.

use crate::ffi::Word;

#[derive(Debug, Clone)]
pub struct Line {
    pub words: Vec<Word>,
    pub y: f64,
    pub size: f64,
    pub x0: f64,
    pub x1: f64,
}

impl Line {
    pub fn text(&self) -> String {
        let mut s = String::new();
        for (i, w) in self.words.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(&w.text);
        }
        s
    }
    pub fn bold(&self) -> bool {
        !self.words.is_empty() && self.words.iter().all(|w| w.bold)
    }
}

#[derive(Debug)]
pub enum Block {
    Para(Vec<Line>),
    Heading(u8, Line),
    Table(Table),
}

#[derive(Debug)]
pub struct Table {
    pub rows: Vec<Vec<String>>,
    pub header: bool,
}

/// Group words into lines by vertical overlap. Poppler emits reading-order
/// words, but rows of a table interleave, so cluster on y explicitly.
pub fn lines(mut words: Vec<Word>) -> Vec<Line> {
    if words.is_empty() {
        return Vec::new();
    }
    words.sort_by(|a, b| {
        a.ymid()
            .partial_cmp(&b.ymid())
            .unwrap()
            .then(a.x0.partial_cmp(&b.x0).unwrap())
    });

    let mut out: Vec<Vec<Word>> = Vec::new();
    for w in words {
        let placed = out.last_mut().is_some_and(|cur| {
            let cy = cur.iter().map(|c| c.ymid()).sum::<f64>() / cur.len() as f64;
            let h = cur.iter().map(|c| c.y1 - c.y0).sum::<f64>() / cur.len() as f64;
            let tol = (h * 0.55).max(1.2);
            if (w.ymid() - cy).abs() <= tol {
                cur.push(w.clone());
                true
            } else {
                false
            }
        });
        if !placed {
            out.push(vec![w]);
        }
    }

    out.into_iter()
        .map(|mut ws| {
            ws.sort_by(|a, b| a.x0.partial_cmp(&b.x0).unwrap());
            let ws = merge_tracking(ws);
            let y = ws.iter().map(|w| w.ymid()).sum::<f64>() / ws.len() as f64;
            let size = median(&mut ws.iter().map(|w| w.size).collect::<Vec<_>>());
            let x0 = ws.iter().map(|w| w.x0).fold(f64::MAX, f64::min);
            let x1 = ws.iter().map(|w| w.x1).fold(f64::MIN, f64::max);
            Line { words: ws, y, size, x0, x1 }
        })
        .collect()
}

/// Rejoin glyph runs split by letter-spacing. The appraisals in the test corpus
/// set headings with tracking, so poppler reports "T ota l Reta i l" as six
/// words; a keyword search for "Total Retail" then finds nothing. Anything
/// closer than a fraction of an em is not a word break.
fn merge_tracking(ws: Vec<Word>) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::with_capacity(ws.len());
    let mut i = 0;
    while i < ws.len() {
        // Collect a run of pieces separated by sub-tracking gaps. A real
        // inter-word space is ~0.25em in most faces; tracking gaps land well
        // below that, and are often negative.
        let mut j = i + 1;
        while j < ws.len() {
            let (p, w) = (&ws[j - 1], &ws[j]);
            let em = p.size.max(w.size).max(1.0);
            let gap = w.x0 - p.x1;
            if gap < em * 0.16 && (w.ymid() - p.ymid()).abs() < em * 0.4 {
                j += 1;
            } else {
                break;
            }
        }

        // Decide on the RUN, not on each pair. Letter-spacing shatters a word
        // into three or more pieces ("O|ffic|e"); a run of exactly two is nearly
        // always two genuine neighbours whose cells happen to nearly touch — a
        // right-aligned "$318" against a left-aligned "12", or "Wholesale" beside
        // "Trade" — and merging those corrupts data rather than repairing it.
        // Two pieces may still join when both are themselves sub-word fragments.
        let n = j - i;
        let piece = |w: &Word| w.text.chars().count() <= 2;
        // A numeric piece is never a tracking fragment worth joining blind: that
        // is how "$318" beside "12" became "$31812". Letters may join on a single
        // stub ("Tota"+"l"), which tracking produces constantly.
        let numeric = ws[i..j]
            .iter()
            .any(|w| w.text.chars().any(|c| c.is_ascii_digit() || c == '$'));
        let two_ok = ws[i..j].iter().any(piece) && !numeric;
        if n >= 3 || (n == 2 && two_ok) {
            let mut m = ws[i].clone();
            for w in &ws[i + 1..j] {
                m.text.push_str(&w.text);
                m.x1 = w.x1;
                m.y0 = m.y0.min(w.y0);
                m.y1 = m.y1.max(w.y1);
            }
            out.push(m);
        } else {
            out.extend_from_slice(&ws[i..j]);
        }
        i = j;
    }
    out
}

pub fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Split a page into reading-order columns by finding a full-height whitespace
/// corridor. Only accepts a split that leaves substantial text on both sides,
/// so a centred heading or a narrow table does not trigger it.
pub fn columns(lines: &[Line], page_w: f64) -> Vec<Vec<usize>> {
    let single = vec![(0..lines.len()).collect::<Vec<_>>()];
    if lines.len() < 8 {
        return single;
    }
    let bins = 200usize;
    let bw = page_w / bins as f64;
    let mut occ = vec![0u32; bins];
    for l in lines {
        for w in &l.words {
            let a = ((w.x0 / bw).floor().max(0.0) as usize).min(bins - 1);
            let b = ((w.x1 / bw).ceil().max(0.0) as usize).min(bins);
            for o in occ.iter_mut().take(b).skip(a) {
                *o += 1;
            }
        }
    }
    // candidate gutters in the middle half of the page
    let (lo, hi) = (bins * 3 / 10, bins * 7 / 10);
    let mut best: Option<(usize, usize)> = None;
    let mut i = lo;
    while i < hi {
        if occ[i] == 0 {
            let s = i;
            while i < hi && occ[i] == 0 {
                i += 1;
            }
            if best.is_none_or(|(bs, be)| i - s > be - bs) {
                best = Some((s, i));
            }
        } else {
            i += 1;
        }
    }
    let Some((s, e)) = best else { return single };
    if (e - s) as f64 * bw < page_w * 0.035 {
        return single;
    }
    let split = (s + e) as f64 * 0.5 * bw;

    // A straddling line is not proof that the page is single-column: papers and
    // reports routinely run a title, a section heading or a full-width table
    // across both columns. Treat each straddler as a band boundary — flush the
    // columns above it, emit it in place, then start a fresh band. Bailing out
    // on the first straddler, as this used to, throws away the whole two-column
    // reading order because of one heading.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let (mut l, mut r): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
    let mut paired = 0usize;
    let flush = |l: &mut Vec<usize>, r: &mut Vec<usize>, g: &mut Vec<Vec<usize>>, paired: &mut usize| {
        if !l.is_empty() && !r.is_empty() {
            *paired += 1;
        }
        // Order matters: the whole left column of a band precedes the right.
        if !l.is_empty() {
            g.push(std::mem::take(l));
        }
        if !r.is_empty() {
            g.push(std::mem::take(r));
        }
    };
    for (i, ln) in lines.iter().enumerate() {
        if ln.x1 <= split {
            l.push(i);
        } else if ln.x0 >= split {
            r.push(i);
        } else {
            flush(&mut l, &mut r, &mut groups, &mut paired);
            groups.push(vec![i]);
        }
    }
    flush(&mut l, &mut r, &mut groups, &mut paired);

    // Only claim a two-column page if some band actually had both columns
    // populated; otherwise this is one column with a ragged right edge.
    let total: usize = groups.iter().map(|g| g.len()).sum();
    if paired == 0 || total < 8 {
        return single;
    }
    groups
}

/// A line looks tabular when it contains internal gaps far wider than *its own*
/// typical word spacing — i.e. it is laid out in cells rather than flowed.
///
/// The comparison has to be relative. Justified prose stretches its spaces to
/// reach the right margin, so an absolute em threshold flags ordinary body text
/// as a table; measuring each gap against the line's median gap does not.
fn tabular(l: &Line) -> bool {
    if l.words.len() < 2 {
        return false;
    }
    let em = l.size.max(1.0);
    let gaps: Vec<f64> = l.words.windows(2).map(|p| (p[1].x0 - p[0].x1).max(0.0)).collect();

    // Two-column financial statements — "5100 Revenue      $12,345,678.90" — carry
    // exactly ONE inter-cell gap, and poppler often returns the label as a single
    // box, so the row has just two words. Both the ">=3 words" and ">=2 wide gaps"
    // rules reject it, which is how an entire P&L reads as prose. A lone gap
    // qualifies only when it is wider than any justified space could stretch, on
    // a line short enough to be a label/value pair — and the corridor test still
    // has to agree before this becomes a table.
    if gaps.iter().any(|g| *g > em * 3.0) && l.words.len() <= 6 {
        return true;
    }

    if l.words.len() < 3 || gaps.len() < 2 {
        return false;
    }
    let mut sorted = gaps.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // The low quartile approximates what an ordinary space looks like on this
    // line. The median would not: in a row whose cells are mostly single words,
    // *most* gaps are inter-cell gaps, so the median is itself a cell gap and
    // nothing ever clears a multiple of it.
    let space = sorted[sorted.len() / 4].max(0.35);

    let bimodal = gaps.iter().filter(|g| **g > space * 2.5 && **g > em * 0.7).count();
    let wide = gaps.iter().filter(|g| **g > em * 1.9).count();
    bimodal >= 2 || wide >= 2
}

/// Whitespace-corridor column boundaries for a run of lines.
fn corridors(rows: &[&Line]) -> Vec<(f64, f64)> {
    let x0 = rows.iter().map(|l| l.x0).fold(f64::MAX, f64::min);
    let x1 = rows.iter().map(|l| l.x1).fold(f64::MIN, f64::max);
    if x1 - x0 < 20.0 {
        return Vec::new();
    }
    let bins = 600usize;
    let bw = (x1 - x0) / bins as f64;
    let mut occ = vec![0u32; bins];
    for l in rows {
        for w in &l.words {
            let a = (((w.x0 - x0) / bw).floor().max(0.0) as usize).min(bins - 1);
            let b = (((w.x1 - x0) / bw).ceil().max(0.0) as usize).min(bins);
            for o in occ.iter_mut().take(b).skip(a) {
                *o += 1;
            }
        }
    }
    let em = median(&mut rows.iter().map(|l| l.size).collect::<Vec<_>>()).max(1.0);
    // This can sit below a single space width because the occupancy test above
    // already does the discriminating: a gap inside a phrase ("International
    // Bank - OTL") falls at a different x on every row, so some row fills it,
    // whereas a real column boundary is clear on every row at the same x. The
    // width floor is only here to shrug off bin quantisation.
    let min_gap = (em * 0.30).max(bw * 2.5);

    // A spanning cell — a "2024 Assessment" title sitting across three money
    // columns — bridges the corridor below it. Requiring strictly zero
    // occupancy would let one such row erase every column boundary under it,
    // so tolerate a few crossings before giving up on a gap.
    // At least one crossing is always tolerated: a short table with a spanning
    // header would otherwise floor the allowance to zero and lose every column
    // boundary, which is precisely the case a big table survives by accident.
    let bridge = (((rows.len() as f64) * 0.15).floor() as u32).max(u32::from(rows.len() >= 3));

    let mut cuts = Vec::new();
    let mut i = 0;
    while i < bins {
        if occ[i] <= bridge {
            let s = i;
            while i < bins && occ[i] <= bridge {
                i += 1;
            }
            let (a, b) = (x0 + s as f64 * bw, x0 + i as f64 * bw);
            if b - a >= min_gap && s > 0 && i < bins {
                cuts.push((a, b));
            }
        } else {
            i += 1;
        }
    }
    // Build column ranges from the cuts.
    let mut cols = Vec::new();
    let mut cur = x0;
    for (a, b) in &cuts {
        cols.push((cur, *a));
        cur = *b;
    }
    cols.push((cur, x1));
    cols.retain(|(a, b)| b - a > bw);
    cols
}

fn build_table(rows: &[&Line]) -> Option<Table> {
    let cols = corridors(rows);
    if cols.len() < 2 {
        return None;
    }
    let mut grid = Vec::with_capacity(rows.len());
    for l in rows {
        let mut cells = vec![String::new(); cols.len()];
        for w in &l.words {
            let mid = (w.x0 + w.x1) * 0.5;
            // nearest column by midpoint, so a word overhanging its cell still lands right
            let ci = cols
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    let da = if mid < a.0 { a.0 - mid } else if mid > a.1 { mid - a.1 } else { 0.0 };
                    let db = if mid < b.0 { b.0 - mid } else if mid > b.1 { mid - b.1 } else { 0.0 };
                    da.partial_cmp(&db).unwrap()
                })
                .map(|(i, _)| i)
                .unwrap_or(0);
            if !cells[ci].is_empty() {
                cells[ci].push(' ');
            }
            cells[ci].push_str(&w.text);
        }
        grid.push(cells);
    }
    // A column that carries a header and no data at all is a label stranded by
    // a corridor: its values are in the column beside it. Send the label there
    // rather than leaving a phantom column and an unlabelled one.
    if grid.len() > 2 {
        let mut c = 0;
        while c < grid[0].len() && grid[0].len() > 1 {
            let empty_body = grid
                .iter()
                .skip(1)
                .all(|r| r.get(c).is_none_or(|v| v.trim().is_empty()));
            let label = grid[0].get(c).map(|v| v.trim().to_string()).unwrap_or_default();
            if empty_body && !label.is_empty() && c + 1 < grid[0].len() {
                let next = grid[0][c + 1].trim().to_string();
                grid[0][c + 1] = if next.is_empty() { label } else { format!("{label} {next}") };
                for r in grid.iter_mut() {
                    if c < r.len() {
                        r.remove(c);
                    }
                }
            } else if empty_body && label.is_empty() {
                for r in grid.iter_mut() {
                    if c < r.len() {
                        r.remove(c);
                    }
                }
            } else {
                c += 1;
            }
        }
    }

    // A single source column can be split by a spurious corridor. A rent roll's
    // "Expiry" column carries right-aligned dates AND left-aligned phrases
    // ("5Y Term starting after vacating"); on the rows that hold only a date,
    // the left half is blank, and that blank runs deep enough to read as a
    // column boundary. The label then sits one column left of its own data, so
    // a model pulling fields out reads the expiry date as the floor area.
    //
    // The tell is that the two halves are essentially never both filled on the
    // same row. Tolerate a few — a "Head Lease" row legitimately carries a term
    // phrase beside an N/A — but not a majority.
    let mut c = 0;
    while c + 1 < grid.first().map(|r| r.len()).unwrap_or(0) {
        let has_hdr = grid.len() > 2;
        let skip = usize::from(has_hdr);
        let filled = |r: &Vec<String>, i: usize| r.get(i).is_some_and(|v| !v.trim().is_empty());
        let n = grid.len() - skip;
        let both = grid.iter().skip(skip).filter(|r| filled(r, c) && filled(r, c + 1)).count();
        let left = grid.iter().skip(skip).filter(|r| filled(r, c)).count();
        let right = grid.iter().skip(skip).filter(|r| filled(r, c + 1)).count();

        if left > 0 && right > 0 && both * 10 <= n {
            // Both header cells named something. The first names the column
            // being merged; the second belongs to the column after it, which is
            // where its data actually sits.
            let mut displaced: Option<String> = None;
            for (ri, r) in grid.iter_mut().enumerate() {
                if c + 1 >= r.len() {
                    continue;
                }
                let a = r[c].trim().to_string();
                let b = r[c + 1].trim().to_string();
                if ri == 0 && has_hdr && !a.is_empty() && !b.is_empty() {
                    displaced = Some(b);
                    r[c + 1] = a;
                } else {
                    r[c + 1] = match (a.is_empty(), b.is_empty()) {
                        (true, _) => b,
                        (_, true) => a,
                        _ => format!("{a} {b}"),
                    };
                }
                r.remove(c);
            }
            if let Some(d) = displaced {
                if let Some(h) = grid.first_mut().and_then(|r| r.get_mut(c + 1)) {
                    *h = if h.trim().is_empty() { d } else { format!("{d} {}", h.trim()) };
                }
            }
        } else {
            c += 1;
        }
    }

    // Accounting layout puts the currency symbol in its own cell, left-aligned
    // against a right-aligned figure, so a corridor opens between them and "$"
    // becomes a column of its own: `| $ | 20.08 |`. Fold any column whose every
    // entry is a bare symbol into the one after it.
    let ncols = grid.first().map(|r| r.len()).unwrap_or(0);
    let mut c = 0;
    while c + 1 < grid.first().map(|r| r.len()).unwrap_or(0) {
        // Judge on the body only: the header cell above a "$" column holds the
        // measure's name ("Base", "Year"), which would otherwise disqualify it.
        let skip = usize::from(grid.len() > 2);
        let entries: Vec<&String> = grid.iter().skip(skip).filter_map(|r| r.get(c)).collect();
        // Tolerate a minority of stragglers: a wrapped header fragment or a
        // footnote marker can land in the symbol column without making it a
        // real column of data.
        let filled: Vec<&&String> = entries.iter().filter(|v| !v.trim().is_empty()).collect();
        let syms = filled
            .iter()
            .filter(|v| matches!(v.trim(), "$" | "-" | "—" | "(" | ")"))
            .count();
        let symbolic = filled.len() >= 2 && syms * 5 >= filled.len() * 4;
        if symbolic {
            for r in grid.iter_mut() {
                let sym = r[c].trim().to_string();
                if !sym.is_empty() && !r[c + 1].trim().is_empty() {
                    r[c + 1] = format!("{sym}{}", r[c + 1].trim());
                } else if !sym.is_empty() {
                    r[c + 1] = sym;
                }
                r.remove(c);
            }
        } else {
            c += 1;
        }
    }
    let _ = ncols;

    // A table whose rows are nearly all single-cell is really prose. Count rows,
    // not cells: a statement's section labels ("Income", "Cost of Goods Sold")
    // each occupy one cell legitimately, and summing cells lets a handful of them
    // drag a perfectly good P&L below the bar and back into a paragraph.
    let multi = grid
        .iter()
        .filter(|r| r.iter().filter(|c| !c.trim().is_empty()).count() >= 2)
        .count();
    if multi * 2 < grid.len() {
        return None;
    }
    // Two-tier headers. A wide table bands its columns under a spanning banner —
    // `Property Description` across three columns, `Unit Details` across six —
    // and the banner takes the header row, leaving the real column labels in the
    // first BODY row. Nothing is lost and every value is present, so a recall
    // metric scores it perfectly; what breaks is the binding a model actually
    // extracts by. It reads the banner as the column's meaning, and reads the
    // labels as a tenant whose Unit Type is "Unit Type".
    //
    // The tell is a header that is mostly empty sitting above a row that is
    // fully populated and carries no figures, over rows that do. Fold the banner
    // INTO the labels rather than dropping either: `Tenant Profile / Tenant`
    // keeps the grouping without costing the field its name.
    if grid.len() > 3 {
        let ncol = grid[0].len();
        let blank_head = grid[0].iter().filter(|c| c.trim().is_empty()).count();
        let filled = grid[1].iter().filter(|c| !c.trim().is_empty()).count();
        let numeric_row = |r: &Vec<String>| r.iter().any(|c| numeric_cell(c));
        // A column label is a name. `Total per Unit`, `Unit Area (SF)`, `% of
        // NRA` all qualify; `1 2 of 14` does not, and neither does a DocuSign
        // stamp — a 4-row block of envelope junk on an ESA appendix page passes
        // every other test here, and folding its rows together welded a page
        // number onto an envelope id. Requiring the labels to carry no figures
        // at all costs a genuine `2025 Actuals` sub-label and buys back every
        // false positive in the corpus, which is the right side to err on: the
        // penalty is leaving a table exactly as it was.
        let wordy = |r: &Vec<String>| !r.iter().any(|c| c.chars().any(|ch| ch.is_ascii_digit()));
        let banded = blank_head * 5 >= ncol * 2      // banner leaves most cells empty
            && filled * 10 >= ncol * 7               // labels fill their row
            && wordy(&grid[1])                       // …and are labels, not data
            && grid.iter().skip(2).any(numeric_row); // …over something measured
        if banded {
            for (i, top) in grid.remove(0).into_iter().enumerate() {
                let top = top.trim();
                if top.is_empty() {
                    continue;
                }
                match grid[0].get_mut(i) {
                    Some(cell) if !cell.trim().is_empty() => {
                        *cell = format!("{top} / {}", cell.trim());
                    }
                    Some(cell) => *cell = top.to_string(),
                    None => {}
                }
            }
        }
    }

    let header = rows.first().is_some_and(|l| l.bold()) || grid.len() > 2;
    Some(Table { rows: grid, header })
}

/// A cell that reads as a figure rather than a label. Deliberately loose:
/// `$1,200`, `(4.5%)` and `12` all count, `Suite 400` does not — a label with a
/// number in it is still a label.
fn numeric_cell(s: &str) -> bool {
    let t = s.trim().trim_matches(|c| matches!(c, '$' | '(' | ')' | '%' | '*' | '-'));
    !t.is_empty()
        && t.chars().any(|c| c.is_ascii_digit())
        && t.chars().all(|c| c.is_ascii_digit() || matches!(c, ',' | '.' | ' '))
}

pub fn blocks(lines: &[Line], body: f64, page_h: f64) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        // --- table run ---
        if tabular(&lines[i]) {
            let mut j = i + 1;
            while j < lines.len() {
                let gap = lines[j].y - lines[j - 1].y;
                let lim = lines[j].size.max(lines[j - 1].size) * 2.6;
                // A financial table is interrupted by section labels ("Retail",
                // "Office Units") and by spanning subtotal rows. Those are part
                // of the table, and breaking the run on them splits one table
                // into fragments that each derive their own, disagreeing,
                // column boundaries. Only flowed prose ends a table.
                //
                // A continuation row is the other trap. Rent rolls merge one set
                // of figures across two tenant rows, so the second row carries a
                // unit and a name and nothing else — too few cells to look
                // tabular, too many words to look like a section label. It is
                // still part of the table, and it says so by starting in the
                // table's own first column.
                let em = lines[j].size.max(1.0);
                let aligned = (lines[j].x0 - lines[i].x0).abs() < em;
                let prose = !tabular(&lines[j]) && lines[j].words.len() > 4 && !aligned;
                if gap > lim || prose {
                    break;
                }
                j += 1;
            }
            if j - i >= 2 {
                let rows: Vec<&Line> = lines[i..j].iter().collect();
                if let Some(t) = build_table(&rows) {
                    out.push(Block::Table(t));
                    i = j;
                    continue;
                }
            }
        }

        // --- heading ---
        let l = &lines[i];
        let short = (l.x1 - l.x0) < 420.0;
        // A wrapped prose line can be short and set slightly above the page's
        // dominant size (a page that is mostly small table type drags the body
        // estimate down), so size alone promotes body text to headings. Require
        // it to also *look* like a heading: few words, not a sentence
        // continuation, and clear of the running head and foot.
        let in_margin = l.y < page_h * 0.06 || l.y > page_h * 0.94;
        let continuation = l.words.first().is_some_and(|w| {
            w.text.chars().next().is_some_and(|c| c.is_lowercase())
        });
        let heading_shape = short
            && !in_margin
            && !continuation
            && l.words.len() <= 14
            && !l.text().trim_end().ends_with(',');
        if heading_shape && (l.size > body * 1.15 || (l.bold() && l.size >= body)) {
            let lvl = if l.size > body * 1.9 {
                1
            } else if l.size > body * 1.45 {
                2
            } else if l.size > body * 1.2 {
                3
            } else {
                4
            };
            out.push(Block::Heading(lvl, l.clone()));
            i += 1;
            continue;
        }

        // --- paragraph ---
        let mut j = i + 1;
        while j < lines.len() {
            let gap = lines[j].y - lines[j - 1].y;
            let lim = lines[j].size.max(lines[j - 1].size) * 1.9;
            if gap > lim || tabular(&lines[j]) || lines[j].size > body * 1.12 {
                break;
            }
            j += 1;
        }
        out.push(Block::Para(lines[i..j].to_vec()));
        i = j;
    }
    out
}

/// Dominant body font size, weighted by how much text is set in it.
pub fn body_size(lines: &[Line]) -> f64 {
    let mut buckets: Vec<(f64, usize)> = Vec::new();
    for l in lines {
        let n: usize = l.words.iter().map(|w| w.text.len()).sum();
        let s = (l.size * 4.0).round() / 4.0;
        match buckets.iter_mut().find(|(b, _)| (*b - s).abs() < 0.13) {
            Some((_, c)) => *c += n,
            None => buckets.push((s, n)),
        }
    }
    buckets
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(s, _)| s)
        .unwrap_or(10.0)
}

/// Text that repeats in the same margin position across most pages: a running
/// head, a footer, a template stamp, a DocuSign envelope id.
///
/// This has to be a document-level decision. Nothing about the line in
/// isolation says "boilerplate" — `Docusign Envelope ID: 4808F9DA…` looks like
/// ordinary content on page 1 and is only revealed as chrome by appearing on
/// all 59. Requiring a consistent margin position keeps a genuinely repeated
/// body sentence (a defined term, a recurring clause) out of the set.
pub fn running_chrome(
    pages: &[(Vec<Line>, f64)],
    min_pages: usize,
    body: f64,
) -> std::collections::HashSet<String> {
    use std::collections::HashMap;
    let mut seen: HashMap<String, Vec<f64>> = HashMap::new();
    for (lines, page_h) in pages {
        if *page_h <= 0.0 {
            continue;
        }
        for l in lines {
            let rel = l.y / page_h;
            if (0.08..=0.92).contains(&rel) {
                continue; // only the margins can hold chrome
            }
            // Chrome is never set larger than body text; a section heading at the
            // top of a page sits in the same band and is NOT chrome. Without this
            // guard "## Appendix A", a photo caption, and the title of a short
            // agreement all get deleted — silent data loss, which is far worse
            // than the boilerplate it was trying to remove.
            if body > 0.0 && l.size > body * 1.02 {
                continue;
            }
            let t = l.text().trim().to_string();
            if t.chars().count() < 8 {
                continue;
            }
            seen.entry(t).or_default().push(rel);
        }
    }
    let need = min_pages.max(3);
    if std::env::var_os("GLEAN_DEBUG_CHROME").is_some() {
        let mut v: Vec<_> = seen.iter().map(|(t, ys)| (ys.len(), t.clone())).collect();
        v.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
        eprintln!("[chrome] candidates in margins: {} (need {})", seen.len(), need);
        for (n, t) in v.iter().take(6) {
            let ys = &seen[t];
            let lo = ys.iter().cloned().fold(f64::MAX, f64::min);
            let hi = ys.iter().cloned().fold(f64::MIN, f64::max);
            eprintln!("  x{n:<4} band {lo:.3}..{hi:.3} spread {:.3}  {}", hi - lo, &t[..t.len().min(52)]);
        }
    }
    seen.into_iter()
        .filter(|(_, ys)| {
            if ys.len() < need {
                return false;
            }
            // same band on every appearance, or it is content that happens to recur
            let lo = ys.iter().cloned().fold(f64::MAX, f64::min);
            let hi = ys.iter().cloned().fold(f64::MIN, f64::max);
            hi - lo < 0.08
        })
        .map(|(t, _)| t)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a line of words at given x positions, all one font size.
    fn line(size: f64, spans: &[(f64, f64, &str)]) -> Line {
        let words: Vec<Word> = spans
            .iter()
            .map(|&(x0, x1, t)| Word {
                x0,
                y0: 0.0,
                x1,
                y1: size,
                size,
                bold: false,
                text: t.to_string(),
            })
            .collect();
        let x0 = words.iter().map(|w| w.x0).fold(f64::MAX, f64::min);
        let x1 = words.iter().map(|w| w.x1).fold(f64::MIN, f64::max);
        Line { words, y: 0.0, size, x0, x1 }
    }

    #[test]
    fn tracking_is_rejoined() {
        // "O ffic e" as poppler reports a letter-spaced heading.
        let ws = line(10.0, &[(0.0, 6.0, "O"), (6.5, 20.0, "ffic"), (20.4, 26.0, "e")]).words;
        let merged = merge_tracking(ws);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].text, "Office");
    }

    #[test]
    fn touching_cells_do_not_weld() {
        // A right-aligned "$318" almost touching a left-aligned "12" in the next
        // column. Two pieces, both real values — merging invents "$31812".
        let ws = line(7.0, &[(400.0, 418.0, "$318"), (418.6, 428.0, "12")]).words;
        let m = merge_tracking(ws);
        assert_eq!(m.len(), 2, "adjacent cells must not weld");
        assert_eq!(m[0].text, "$318");
    }

    #[test]
    fn adjacent_words_do_not_weld() {
        // "Wholesale Trade" set tight in a table label.
        let ws = line(8.0, &[(0.0, 44.0, "Wholesale"), (44.7, 70.0, "Trade")]).words;
        assert_eq!(merge_tracking(ws).len(), 2, "two real words must not weld");
    }

    #[test]
    fn two_piece_letter_split_still_joins() {
        // "Tota l" — tracking leaves a one-letter stub. Letters may join on a
        // single stub; digits may not.
        let ws = line(7.0, &[(0.0, 22.0, "Tota"), (22.5, 26.0, "l")]).words;
        let m = merge_tracking(ws);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].text, "Total");
    }

    #[test]
    fn real_word_breaks_survive() {
        // A normal space is far wider than a tracking gap and must not merge.
        let ws = line(10.0, &[(0.0, 20.0, "Total"), (23.0, 45.0, "Retail")]).words;
        assert_eq!(merge_tracking(ws).len(), 2);
    }

    #[test]
    fn justified_prose_is_not_a_table() {
        // Stretched spaces in justified body text: uniform, ~1.4x a normal space.
        let l = line(
            9.0,
            &[(0.0, 30.0, "The"), (34.0, 70.0, "assessed"), (74.0, 100.0, "value"),
              (104.0, 130.0, "is"), (134.0, 170.0, "the")],
        );
        assert!(!tabular(&l), "justified prose must not read as tabular");
    }

    #[test]
    fn cell_row_is_a_table() {
        // "311 Marlow Road   311   00593101   $1,250,000" — tight inside a cell,
        // wide between cells.
        let l = line(
            7.0,
            &[(0.0, 12.0, "311"), (13.0, 40.0, "North"), (41.0, 60.0, "Road"),
              (100.0, 112.0, "311"), (150.0, 190.0, "00593101"), (240.0, 290.0, "$1,250,000")],
        );
        assert!(tabular(&l), "a cell-laid-out row must read as tabular");
    }

    #[test]
    fn two_column_statement_is_a_table() {
        // A P&L line: label at the left margin, amount right-aligned far away.
        // Only one gap exists, so a ">=2 wide gaps" rule reads it as prose.
        let l = line(8.0, &[(50.0, 78.0, "4200"), (80.0, 110.0, "Sales"),
                            (480.0, 560.0, "$12,345,678.90")]);
        assert!(tabular(&l), "a two-column statement row must read as tabular");
        // poppler often returns the whole label as one box, leaving two words.
        let two = line(8.0, &[(50.0, 110.0, "5100 Revenue"), (480.0, 560.0, "$12,345,678.90")]);
        assert!(tabular(&two), "a two-WORD statement row must read as tabular");
    }

    /// Build a table from rows of (x0, x1, text) and return the emitted grid.
    fn grid(size: f64, rows: &[Vec<(f64, f64, &str)>]) -> Vec<Vec<String>> {
        let ls: Vec<Line> = rows.iter().map(|r| line(size, r)).collect();
        let refs: Vec<&Line> = ls.iter().collect();
        build_table(&refs).expect("expected a table").rows
    }

    #[test]
    fn currency_symbol_column_folds_into_its_figure() {
        // Accounting layout: "$" left-aligned in its own cell, figure right
        // aligned in the next. A corridor opens between them.
        let g = grid(8.0, &[
            vec![(0.0, 40.0, "Unit"), (120.0, 160.0, "Base")],
            vec![(0.0, 30.0, "2-280"), (120.0, 128.0, "$"), (170.0, 210.0, "20.08")],
            vec![(0.0, 30.0, "1-382"), (120.0, 128.0, "$"), (170.0, 210.0, "25.37")],
            vec![(0.0, 30.0, "1-374"), (120.0, 128.0, "$"), (170.0, 210.0, "40.05")],
        ]);
        let body: Vec<&Vec<String>> = g.iter().skip(1).collect();
        for r in &body {
            assert!(
                !r.iter().any(|c| c.trim() == "$"),
                "a bare $ must not survive as its own cell: {r:?}"
            );
        }
        assert!(
            body.iter().any(|r| r.iter().any(|c| c.trim() == "$20.08")),
            "expected $20.08, got {body:?}"
        );
    }

    #[test]
    fn a_split_column_rejoins_and_takes_its_label_with_it() {
        // The rent-roll case, to scale: "Expiry" is left-aligned at the start of
        // a wide column whose dates are right-aligned at its end, so a blank
        // corridor opens between them. Left split, the label sits one column
        // away from its own data.
        let g = grid(8.0, &[
            vec![(0.0, 30.0, "Unit"), (255.0, 274.0, "Expiry"),
                 (450.0, 470.0, "Size"), (520.0, 550.0, "Base")],
            vec![(0.0, 30.0, "2-280"), (380.0, 420.0, "12/31/2026"),
                 (452.0, 470.0, "718"), (520.0, 550.0, "20.08")],
            vec![(0.0, 30.0, "1-382"), (380.0, 420.0, "5/31/2026"),
                 (450.0, 470.0, "1,615"), (520.0, 550.0, "25.37")],
            vec![(0.0, 30.0, "1-374"), (380.0, 420.0, "9/30/2026"),
                 (450.0, 470.0, "1,719"), (520.0, 550.0, "40.05")],
        ]);
        let hdr = &g[0];
        let row = &g[1];
        let expiry = hdr.iter().position(|h| h.contains("Expiry")).expect("Expiry header");
        assert_eq!(
            row[expiry].trim(),
            "12/31/2026",
            "the expiry date must sit under the Expiry label, got {g:?}"
        );
        let size = hdr.iter().position(|h| h.contains("Size")).expect("Size header");
        assert_eq!(row[size].trim(), "718", "size must sit under Size, got {g:?}");
    }

    #[test]
    fn merged_figures_keep_the_continuation_row_in_the_table() {
        // A rent roll runs one set of figures across two tenant rows; the second
        // carries a unit and a name only. It belongs to the table.
        let rows = vec![
            line(8.0, &[(0.0, 30.0, "4-400"), (40.0, 200.0, "Medispa Westmount 1"),
                        (300.0, 340.0, "7,889"), (400.0, 460.0, "143,343.13")]),
            line(8.0, &[(0.0, 30.0, "400"), (40.0, 200.0, "Medispa Westmount 2")]),
            line(8.0, &[(0.0, 30.0, "4-450"), (40.0, 200.0, "Sotheby's Realty"),
                        (300.0, 340.0, "585"), (400.0, 460.0, "8,347.95")]),
        ];
        let blocks = blocks(&rows, 8.0, 800.0);
        let tables: Vec<_> = blocks.iter().filter(|b| matches!(b, Block::Table(_))).collect();
        assert_eq!(tables.len(), 1, "the continuation row must not split the table");
        if let Some(Block::Table(t)) = tables.first().copied() {
            assert_eq!(t.rows.len(), 3, "all three rows belong to one table");
        }
    }

    #[test]
    fn chrome_is_found_but_headings_are_spared() {
        let page_h = 800.0;
        let mut pages = Vec::new();
        for _ in 0..8 {
            pages.push((
                vec![
                    // running footer: body size, deep in the margin
                    line(8.0, &[(0.0, 200.0, "Docusign Envelope ID: 4808F9DA")]),
                    // section heading: same band, but LARGER than body
                    line(16.0, &[(0.0, 120.0, "Appendix A")]),
                ],
                page_h,
            ));
        }
        // put them in the top margin
        for (ls, _) in pages.iter_mut() {
            for l in ls.iter_mut() {
                l.y = 20.0;
            }
        }
        let set = running_chrome(&pages, 2, 8.0);
        assert!(set.contains("Docusign Envelope ID: 4808F9DA"), "footer must be chrome");
        assert!(!set.contains("Appendix A"), "a heading must never be chrome: {set:?}");
    }

    #[test]
    fn degenerate_grid_is_not_emitted_as_a_table() {
        // Two aligned words and nothing else is a layout accident.
        let ls = [
            line(8.0, &[(0.0, 20.0, "a"), (200.0, 220.0, "b")]),
            line(8.0, &[(0.0, 20.0, "c")]),
        ];
        let refs: Vec<&Line> = ls.iter().collect();
        if let Some(t) = build_table(&refs) {
            let mut out = String::new();
            crate::md::emit(&mut out, &Block::Table(t));
            assert!(!out.contains("---"), "degenerate grid must not emit a table: {out:?}");
        }
    }

    #[test]
    fn spanning_header_does_not_erase_columns() {
        // One title spans three money columns; the rows below share clean gaps.
        // Requiring strictly-zero occupancy would collapse all three into one.
        let span = line(7.0, &[(100.0, 290.0, "2024 Assessment")]);
        let r1 = line(7.0, &[(100.0, 140.0, "$1,250,000"), (180.0, 220.0, "$417,000"),
                             (250.0, 290.0, "$1,667,000")]);
        let r2 = line(7.0, &[(100.0, 140.0, "$1,252,000"), (180.0, 220.0, "$418,000"),
                             (250.0, 290.0, "$1,670,000")]);
        let r3 = line(7.0, &[(100.0, 140.0, "$1,600,000"), (180.0, 220.0, "$533,000"),
                             (250.0, 290.0, "$2,133,000")]);
        let rows: Vec<&Line> = vec![&span, &r1, &r2, &r3];
        assert!(corridors(&rows).len() >= 3, "spanning cell must not collapse columns");
    }

    #[test]
    fn empty_cells_keep_their_column() {
        // A tenant with no renewal option leaves a hole; later columns must not
        // shift left into it.
        let r1 = line(7.0, &[(0.0, 20.0, "506"), (40.0, 90.0, "Bank"),
                             (120.0, 150.0, "$49.00"), (200.0, 240.0, "Dec")]);
        let r2 = line(7.0, &[(0.0, 20.0, "512"), (40.0, 90.0, "Seed"),
                             (120.0, 150.0, "$68.00"), (200.0, 240.0, "Feb")]);
        let r3 = line(7.0, &[(0.0, 20.0, "500"), (40.0, 90.0, "Northwind"),
                             (120.0, 150.0, "$41.00")]);
        let t = build_table(&[&r1, &r2, &r3]).expect("should build a table");
        assert_eq!(t.rows[2].len(), t.rows[0].len());
        assert_eq!(t.rows[2].last().map(|s| s.as_str()), Some(""));
        assert_eq!(t.rows[0].last().map(|s| s.as_str()), Some("Dec"));
    }

    #[test]
    fn a_banner_row_does_not_take_the_labels_place() {
        // `Property Description` and `Tenant Profile` band the columns; the real
        // labels are the row beneath. Left alone, the header carries the banner
        // and the labels are emitted as a tenant whose Class is "Class".
        let banner = line(7.0, &[(0.0, 20.0, "Description"), (120.0, 150.0, "Profile")]);
        let labels = line(7.0, &[(0.0, 20.0, "Class"), (40.0, 90.0, "Grade"),
                                 (120.0, 150.0, "Tenant"), (200.0, 240.0, "Area")]);
        let r1 = line(7.0, &[(0.0, 20.0, "B"), (40.0, 90.0, "A"),
                             (120.0, 150.0, "Rogers"), (200.0, 240.0, "5,200")]);
        let r2 = line(7.0, &[(0.0, 20.0, "C"), (40.0, 90.0, "B"),
                             (120.0, 150.0, "Thuet"), (200.0, 240.0, "1,100")]);
        let t = build_table(&[&banner, &labels, &r1, &r2]).expect("should build a table");
        assert_eq!(t.rows[0][0], "Description / Class");
        assert_eq!(t.rows[0][2], "Profile / Tenant");
        assert_eq!(t.rows[0][3], "Area");
        assert_eq!(t.rows[1][0], "B", "the data must still start at row 1");
    }

    #[test]
    fn a_banner_over_real_data_is_left_alone() {
        // The other appraisal table: the same banner shape, but the row beneath
        // it is DATA, not labels. Folding it would weld a value onto a heading.
        let banner = line(7.0, &[(0.0, 20.0, "Description"), (120.0, 150.0, "Summary")]);
        let r1 = line(7.0, &[(0.0, 20.0, "Site"), (40.0, 90.0, "1.2"),
                             (120.0, 150.0, "Value"), (200.0, 240.0, "67,100")]);
        let r2 = line(7.0, &[(0.0, 20.0, "Floors"), (40.0, 90.0, "6"),
                             (120.0, 150.0, "Rate"), (200.0, 240.0, "5,200")]);
        let r3 = line(7.0, &[(0.0, 20.0, "Units"), (40.0, 90.0, "34"),
                             (120.0, 150.0, "Term"), (200.0, 240.0, "1,100")]);
        let t = build_table(&[&banner, &r1, &r2, &r3]).expect("should build a table");
        assert_eq!(t.rows[0][0], "Description");
        assert_eq!(t.rows[1][0], "Site");
    }
}
