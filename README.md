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
glean report.pdf --images ./figs     # extract embedded images as PNG
glean report.pdf --images ./figs --figures   # also rasterise vector charts
glean report.pdf --keep-chrome       # keep running heads/footers
glean report.pdf --stats             # page/word/table counts to stderr
```

### Page-addressable output

`--json` emits one record per page, deliberately shaped like a hosted OCR API's
response so a pipeline written against one can consume the other unchanged:

```json
{
  "pages": [
    {"page": 1, "anchor": "page-1", "width": 720.0, "height": 540.0,
     "has_text": true, "markdown": "# ...",
     "images": [{"path": "figs/p001-03.png", "width": 1033, "height": 738,
                 "bbox": [284.4, 209.6, 679.4, 491.7], "page_fraction": 0.287}]}
  ],
  "full_markdown": "...",
  "meta": {"source": "report.pdf", "pages": 10, "images": 29}
}
```

`has_text` is the field to route on. A page with `has_text: false` carrying one
image at `page_fraction: 1.0` is a scan — send that page to OCR. Nothing else in
the output distinguishes it, which is exactly the silent-truncation trap.

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

## What it does not do

**No OCR.** glean reads the text layer. Scanned pages have none, and it says so
on stderr with a count, then exits non-zero if *every* page was scanned:

```
glean: warning: 42 of 110 page(s) have no text layer and were skipped; they need OCR
```

That warning exists because the alternative is what anydoc does on a mixed
document: process the text pages, silently drop 42 scanned ones, and exit 0. For
the ESA in the test corpus those 42 pages held the historical contamination
evidence. Route them to an OCR engine; glean will not pretend they were not there.

No formulas, no reading of embedded attachments. `--images` extracts embedded
rasters, but a vector chart drawn with path operators is not an image and will
not be captured.

## Tests

```sh
cargo test
```

Fourteen tests pin the layout heuristics, each one standing on a bug that
actually shipped:

- tracking repair and both failure directions — a two-piece letter split must
  join, two touching cells and two real words must not
- justified prose vs. cell rows; two-column statements
- spanning headers must not erase the columns beneath them
- empty cells hold their column instead of shifting later ones left
- a currency symbol never survives as its own cell
- a continuation row stays in its table
- a running footer is chrome, a section heading in the same band is not
- a degenerate grid is emitted as text, not as `| | |`

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
