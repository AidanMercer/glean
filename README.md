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

Needs poppler with development headers (`poppler-cpp`), a C++17 compiler, and Rust.

```sh
sudo pacman -S poppler          # Arch; libpoppler-cpp-dev on Debian/Ubuntu
cargo build --release
```

## Use

```sh
glean report.pdf                    # Markdown to stdout
glean report.pdf -o report.md       # or to a file
glean report.pdf -p 32,74 -j 8      # only these pages, 8 threads
glean report.pdf --stats            # page/word/table counts to stderr
```

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
   order, rejected if any line straddles it.
4. **Tables.** Column boundaries come from *vertical whitespace corridors* — x
   ranges no word crosses on any row of the block. This is the core idea. It
   handles ruled and unruled tables identically, and unlike x-anchor clustering
   it copes with empty cells: a tenant with no renewal option leaves a hole
   rather than shifting every later column left.
5. **Markdown.** Never emits a ragged table, never welds two cells together.

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

No formulas, no image extraction, no reading of embedded attachments.

## Tests

```sh
cargo test
```

Six tests pin the layout heuristics: tracking repair, word-break preservation,
justified prose vs. cell rows, spanning headers, and empty-cell column stability.
