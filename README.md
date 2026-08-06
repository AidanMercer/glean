# glean

PDF to GitHub-Flavored Markdown, for feeding documents to an LLM.

Built after benchmarking six existing parsers over 307 pages of real commercial
leases, an appraisal and a Phase 1 environmental site assessment. The two things
that went wrong in that benchmark drove the whole design:

- Borderless financial tables — rent rolls, assessment rolls — collapsed in every
  fast parser, taking their numbers with them.
- The one engine that rebuilt those tables (a hosted VLM) occasionally **invented**
  a value: it turned `$142,500` into `$145,500` and a `Lease Expiry` header into
  `Event End(s)`, with nothing in the output to signal it.

glean is built so neither can happen: it never guesses at a glyph, and it treats
the table grid as the thing worth getting right.

## Results on that corpus

307 pages, 6 documents, same machine, throttled to 6 cores.

| Engine | Time | $ recall | Invented | Ragged tables |
|---|---|---|---|---|
| **glean** | **1.0 s** | **888/888 · 100%** | **0** | **0** |
| anydoc (Rust) | 2.9 s | 335 · 38% | 1 | 9 |
| PyMuPDF4LLM | 73.7 s | 885 · 100% | 1 | 0 |
| pdfplumber | 23.6 s | 831 · 94% | 0 | 0 |
| pypdf | 17.8 s | 853 · 96% | 0 | 0 |
| MarkItDown | 21.5 s | 812 · 91% | 1 | 3 |
| Mistral OCR (hosted) | 47.3 s | 876 · 99% | 6 | 1 |

*$ recall* — currency amounts recovered against every `$` figure in the PDF text
layer. *Invented* — values whose digits appear nowhere in the source, checked
space-insensitively. **71× faster than PyMuPDF4LLM at equal-or-better fidelity.**

On the appraisal alone — the table-heavy document that separates the field —
glean recovers **815/815** and emits 52 well-formed tables, more than any other
engine tested.

## Install

Needs poppler with development headers, a C++20 compiler, zlib, and Rust.
Image extraction uses poppler's core `OutputDev`, so the private headers are
required too — the `poppler` package ships them on Arch.

```sh
sudo pacman -S poppler          # Arch; libpoppler-cpp-dev on Debian/Ubuntu
cargo build --release
```

## Use

```sh
glean report.pdf                     # Markdown to stdout
glean report.pdf -o report.md        # or to a file
glean report.pdf -p 32,74 -j 8       # only these pages, 8 threads
glean report.pdf --json              # page-addressable JSON
glean report.pdf --json --cells      # …with a ref and a box per table cell
glean report.pdf --images ./figs     # extract embedded images as PNG
glean report.pdf --images ./figs --figures   # also rasterise vector charts
glean report.pdf --ocr-pages ./scans # write the scanned pages out for OCR
glean report.pdf --keep-chrome       # keep running heads/footers
glean report.pdf --front-matter --page-marks   # for LLM field extraction
glean report.pdf --stats             # page/word/table counts to stderr
```

### Page-addressable output

`--json` emits one record per page, deliberately shaped like a hosted OCR API's
response so a pipeline written against one can consume the other unchanged:

```json
{
  "pages": [
    {"page": 1, "anchor": "page-1", "width": 720.0, "height": 540.0,
     "has_text": true, "kind": "text", "markdown": "# ...",
     "images": [{"path": "figs/p001-03.png", "width": 1033, "height": 738,
                 "bbox": [284.4, 209.6, 679.4, 491.7], "page_fraction": 0.287}]}
  ],
  "full_markdown": "...",
  "meta": {"source": "report.pdf", "pages": 10, "pages_included": 10,
           "unreadable_pages": 0, "scanned_pages": [], "blank_pages": 0,
           "images": 29}
}
```

`kind` is the field to route on:

| kind | what it is | what to do |
|---|---|---|
| `text` | read from the text layer | nothing |
| `scan` | a page-sized raster, no text | send this page to OCR |
| `image` | a figure or chart, no text | `--images` if you want the picture |
| `blank` | no text, no ink, no raster | nothing — nothing is missing |

Only `scan` costs money downstream, and on a mixed document it is usually a
small minority: a 19-page credit agreement in the test set is one text page and
18 scans, and a 4-page site-visit report is one text page and three
photographs — which need no OCR at all, and were reported as needing it until
the classifier could tell them apart.

`full_markdown` is the same string the Markdown mode writes, front matter and
page marks included when those flags are set. `pages` is the length of the
**document**; `pages_included` is how much of it is in this output.

### Cell provenance

`--cells` gives every table cell a page-anchored ref and the box it occupies:

```json
"tables": [{"table": 1, "rows": 62, "cols": 21, "cells": [
  {"ref": "p2!t1!r5c8", "row": 5, "col": 8, "text": "$1,905",
   "bbox": [355.1, 128.4, 379.6, 135.6]}
]}]
```

A value pulled out of a spreadsheet can say it came from `Sheet1!B12`. A value
pulled out of a PDF could only ever say "page 4" — so anything checking the
extraction had to search the whole page for it, and a figure that appears twice
on that page could not be told apart. `r5c8` is the same class of citation, and
the box lets a reader go and look.

Rows and columns are 1-based and address the table **as emitted** — after
padding, blank rows and blank edge columns are gone — because that is the grid
the reader is looking at. Row 1 is the header. Empty cells are omitted: they
hold nothing to cite, and a box invented for them would be invented evidence.

Checked by cropping each box out of the PDF with `pdftotext` — a different code
path from the one glean builds its grid with — **1,826 of 1,841 sampled cells
(99.2%) contain the text they claim to**. The residue is DocuSign overlay
artifacts whose text `pdftotext` will not return from any crop.

Opt-in, because it is several cells of JSON per value and no use to a consumer
that only wants the prose.

### Images

`--images DIR` writes every embedded image as 8-bit RGB PNG, whatever the source
encoding, and records where it sat on the page. Identical pixels are written
once — a logo repeated on 29 placements becomes one file with 29 recorded
positions. `--figures` additionally finds charts drawn with path operators
(which are not images and never reach an image hook), clusters the ink, and
rasterises those regions at 150 dpi; page-sized clusters are rejected as page
furniture rather than figures. `page_fraction` separates a
figure from a scan backdrop; `--min-image` (default 64px) drops rules and
spacers. Without the flag no images are written and the Markdown carries no image
noise.

## How it works

Extraction goes through poppler — the engine behind `pdftotext` — so glyph
decoding, encodings and CID font handling are already correct. Everything above
that is Rust:

1. **Lines.** Words are clustered by vertical overlap rather than trusting
   emission order, because table rows interleave.
2. **Tracking repair.** Letter-spaced type reaches the text layer as
   `O ffic e Units`. Fragments closer than 0.16 em are rejoined, so a search for
   `Office Units` finds it. This is a repair, never a guess — glean only ever
   joins glyphs that are present.
3. **Columns.** A full-height whitespace gutter splits the page into reading
   order. A line straddling the gutter — a title, a section heading, a
   full-width table — is a band boundary, not a disproof: the columns above it
   are flushed, it is emitted in place, and a fresh band begins.
4. **Running chrome.** Text repeating in the same margin band across an eighth
   of the pages is a running head or footer and is dropped (`--keep-chrome`
   keeps it). This is necessarily a document-level decision: nothing about
   `Docusign Envelope ID: …` marks it as chrome until you see it on all 59
   pages. Worth 1.4–3.1% of a long document, and more in retrieval quality —
   a chunk containing only an envelope ID is noise that still competes for
   your top-k.
5. **Tables.** Column boundaries come from *vertical whitespace corridors* — x
   ranges no word crosses on any row of the block. This is the core idea. It
   handles ruled and unruled tables identically, and unlike x-anchor clustering
   it copes with empty cells: a tenant with no renewal option leaves a hole
   rather than shifting every later column left.
6. **Markdown.** Never emits a ragged table, never welds two cells together.

### Two details that took a while to get right

**A table row's median gap is a cell gap.** Deciding "is this line tabular?" by
comparing gaps to the line's median fails on exactly the rows it needs to catch:
when most cells are single words, most gaps *are* inter-cell gaps, so nothing
clears a multiple of the median. glean uses the low quartile as its estimate of
an ordinary space. Comparing against an absolute em threshold fails the other
way, flagging justified prose as a table.

**A spanning header erases the columns beneath it.** A `2024 Assessment` title
sitting across three money columns crosses every corridor under it. Requiring
strictly-zero occupancy lets that one row destroy the whole grid, so a few
crossings are tolerated — with a floor of one, or short tables lose the
allowance to rounding and fail where long ones survive by luck.

**A banner is not a column label.** A wide table bands its columns under a
spanning title — `Tenant Profile` over three, `Unit Details` over six — and that
banner takes the Markdown header row, leaving the real labels in the first
*body* row. Nothing is lost and every value is present, so recall scores it
perfectly; what breaks is the binding a model extracts by. It reads the banner
as the column's meaning and reads the labels as a tenant whose Unit Type is
"Unit Type". The two rows are folded together instead — `Tenant Profile /
Tenant` — which keeps the grouping without costing the field its name. On a
credit summary with a stacked header this is the difference between `Gross
Commitment` and `Net Commitment` and two columns both labelled `Commitment`.

The tell is a mostly-empty header over a fully-populated row of labels, over
rows that carry figures — and the labels must carry no figures themselves. That
last condition is doing real work: without it a four-row block of DocuSign
envelope junk on an ESA appendix page passes every other test, and folding its
rows welds a page number onto an envelope id. Across 3,068 tables from 61 real
deal documents this fires on 19 and costs 0 recall.

## What it does not do

**No OCR.** glean reads the text layer. Scanned pages have none, and it says so
on stderr — naming them, so the reply is one command and not a hunt:

```
glean: warning: 18 of 19 page(s) are scans with no text layer and were skipped;
they need OCR: 1-13, 15-19
```

That warning exists because the alternative is what anydoc does on a mixed
document: process the text pages, silently drop the scanned ones, and exit 0.
Route them to an OCR engine; glean will not pretend they were not there.

`--ocr-pages DIR` is the reply, in the same run that raises the warning: one
`page-NNNN.png` per scanned page, ready to post. Each is rendered at the
resolution its own scan was captured at — a 300 dpi fax re-rendered at 150 loses
half the ink OCR has to read, and at 600 it invents nothing and quadruples the
bytes — bounded to 150–400 dpi, `--ocr-dpi` to override. Whole pages are
rendered rather than the embedded raster lifted, because a scanned page is
sometimes several tiles and often carries a stamp or a signature drawn over the
scan.

It names only the scans. A page with no text is not necessarily a page with
something missing — it can be a photograph, a chart drawn in vector, or a blank
separator sheet, and telling a model that a blank page "requires OCR" is its own
small lie. Classifying costs one content-stream walk over the wordless pages
with no pixel decoding, and only when there are any: 11 ms on a 54-page scanned
lease, nothing at all on a document with a text layer throughout.

No formulas, no reading of embedded attachments. `--images` extracts embedded
rasters, but a vector chart drawn with path operators is not an image and will
not be captured.

## Tests

```sh
cargo test
```

Twenty-eight tests pin the layout heuristics and the front matter, each one
standing on a bug that actually shipped:

- tracking repair and both failure directions — a two-piece letter split must
  join, two touching cells and two real words must not
- justified prose vs. cell rows; two-column statements
- spanning headers must not erase the columns beneath them
- empty cells hold their column instead of shifting later ones left
- a currency symbol never survives as its own cell
- a continuation row stays in its table
- a running footer is chrome, a section heading in the same band is not
- a degenerate grid is emitted as text, not as `| | |`
- a page subset states the document's length, not the subset's
- only scans are reported as needing OCR — not figures, not blank pages
- a page-sized raster is a scan and a small one is not; vector ink is neither
- a source path containing a colon stays one YAML field
- a banner row does not take the column labels' place — and a banner over real
  data is left alone
- a scan renders at its own resolution; a thumbnail does not drag it up
- a folded column keeps the box of what it absorbed, and the geometry keeps the
  shape of the text through every repair

### Feeding an LLM

Two flags exist for the field-extraction case:

`--front-matter` prepends the source, the length of the document, and — the point
of it — every way this copy of it is incomplete:

```yaml
---
source: "2026.05.22 SIGNED CSMC CCL.pdf"
pages: 19
unreadable_pages: 18
scanned_pages: "1-13, 15-19"
warnings:
  - "18 page(s) are scans with no text layer and are absent from this document; they require OCR: 1-13, 15-19"
---
```

A model asked to pull fields out cannot otherwise distinguish *"this report
contains no contamination findings"* from *"the findings were on 42 pages that
are missing from your context"*. It will answer confidently either way. This
puts the difference in the context window.

There are two ways to hand a model a hole, and the second one is quieter. `-p`
gives it part of a document; if the front matter then reports the size of the
*slice*, a two-page extract of a 33-page deal reads as a two-page deal. So the
document's real length is always stated, and a subset says so:

```yaml
---
source: "deal.pdf"
pages: 33
pages_included: 2
included: "2, 4"
warnings:
  - "31 of 33 pages are not in this document; only page(s) 2, 4 were converted"
---
```

`--page-marks` writes `<!-- page N -->` at each boundary so an extracted value
can cite the page it came from, and so chunking can split on page lines.

Both flags apply to `--json` as well, where they shape `full_markdown` — the
field a pipeline actually hands to a model.

### Rent rolls specifically

Rent rolls are the document this was hardened against, because they are the ones
whose errors reach a valuation. Three of them have been checked cell-by-cell
against **rendered page images** rather than against the text layer — the only
check that does not run through the same engine glean extracts with. All values
matched, including the cells that are legitimately empty: a tenant with no
step-up, another with no renewal option, a pair of tenants whose figures are
merged across two rows.

## License

MIT — see [LICENSE](LICENSE).
