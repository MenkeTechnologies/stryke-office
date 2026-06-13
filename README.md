```
 ███████╗████████╗██████╗ ██╗   ██╗██╗  ██╗███████╗
 ██╔════╝╚══██╔══╝██╔══██╗╚██╗ ██╔╝██║ ██╔╝██╔════╝
 ███████╗   ██║   ██████╔╝ ╚████╔╝ █████╔╝ █████╗
 ╚════██║   ██║   ██╔══██╗  ╚██╔╝  ██╔═██╗ ██╔══╝
 ███████║   ██║   ██║  ██║   ██║   ██║  ██╗███████╗
 ╚══════╝   ╚═╝   ╚═╝  ╚═╝   ╚═╝   ╚═╝  ╚═╝╚══════╝
                  [ o f f i c e ]
```

[![CI](https://github.com/MenkeTechnologies/stryke-office/actions/workflows/ci.yml/badge.svg)](https://github.com/MenkeTechnologies/stryke-office/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![stryke](https://img.shields.io/badge/stryke-package-cyan.svg)](https://github.com/MenkeTechnologies/strykelang)

### `[OFFICE DOCUMENT I/O FOR STRYKE // EXCEL + WORD + POWERPOINT + ODF + PDF]`

> *"Read and write the whole office suite — no LibreOffice required."*

Office document import/export for stryke. Read and write Excel/Calc
(`xlsx`/`ods`), Word/Writer (`docx`/`odt`), PowerPoint/Impress
(`pptx`/`odp`), and PDF — **entirely in native Rust**. There is no
LibreOffice / `soffice` / pandoc subprocess; nothing external has to be
installed. Opt-in package tier, kept out of the stryke core binary.

### [`strykelang`](https://github.com/MenkeTechnologies/strykelang) &middot; [`MenkeTechnologiesMeta`](https://github.com/MenkeTechnologies/MenkeTechnologiesMeta) · [`stryke-polars`](https://github.com/MenkeTechnologies/stryke-polars) · [`stryke-demo`](https://github.com/MenkeTechnologies/stryke-demo)

---

## Table of Contents

- [\[0x00\] Why this is a package, not a builtin](#0x00-why-this-is-a-package-not-a-builtin)
- [\[0x01\] Install](#0x01-install)
- [\[0x02\] Quick start](#0x02-quick-start)
- [\[0x03\] Format matrix](#0x03-format-matrix)
- [\[0x04\] API reference](#0x04-api-reference)
- [\[0x05\] No external binaries](#0x05-no-external-binaries)
- [\[0x06\] Tests](#0x06-tests)
- [\[0x07\] Layout](#0x07-layout)
- [\[0x08\] Roadmap](#0x08-roadmap)
- [\[0xFF\] License](#0xff-license)

---

## [0x00] Why this is a package, not a builtin

Office I/O drags in heavyweight format crates — spreadsheet, word-processor,
presentation, and PDF engines. That belongs in an opt-in package, not the
daily-driver core. `stryke-office` ships a thin stryke library plus a Rust
cdylib (`libstryke_office.{dylib,so}`) dlopened in-process on first
`use Office`. Everything is pure Rust and statically linked, so the cdylib
is self-contained.

## [0x01] Install

From a release tarball:

```sh
s pkg install -g github.com/MenkeTechnologies/stryke-office
```

From a local checkout:

```sh
cd ~/projects/stryke-office
cargo build --release
s pkg install -g .            # cdylib lands in ~/.stryke/store/office@<version>/
```

Or `make install`.

## [0x02] Quick start

```stryke
use Office

# Spreadsheet: write xlsx, read it back, re-emit as ods
Office::sheet_write("report.xlsx", [
    { name => "Sales", rows => [["product", "units"], ["widget", 120]] },
])
val $sheets = Office::sheet_read("report.xlsx")
Office::sheet_write("report.ods", $sheets)

# Document: build a docx from structured blocks
Office::doc_write("memo.docx", [
    { kind => "heading", level => 1, text => "Quarterly Report" },
    { kind => "para", text => "Revenue grew 18%." },
])
val @paragraphs = Office::doc_read("memo.docx")

# Presentation: build a deck
Office::slides_write("deck.pptx", [
    { title => "Intro", body => ["point one", "point two"] },
])

# PDF: generate and extract text (self-contained, no font files)
Office::pdf_write("out.pdf", ["Line one", "Line two"])
val $info = Office::pdf_read("out.pdf")   # { pages => [...], text => "..." }
```

## [0x03] Format matrix

| Kind | Formats | Read | Write |
|---|---|---|---|
| Spreadsheet | xlsx, ods, xls, csv | yes | xlsx, ods |
| Document | docx, odt | yes | yes |
| Presentation | pptx, odp | yes | yes |
| PDF | pdf | text + pages | text |
| Image | png, jpeg, gif, bmp, webp, tiff, … | yes | yes |

The output format is taken from the path extension; override with
`format => "..."`.

Images get a full PIL-style surface (open/new/save/info, resize/thumbnail/
crop/rotate/flip/convert/paste, get/put pixel, and ImageDraw-style
rect/line/circle/text). This complements the stryke core `image_*` filter
builtins (blur, edge, sharpen, grayscale, …) — those operate on pixel
data; this package adds the file I/O and manipulation surface.

## [0x04] API reference

| Function | Returns | Notes |
|---|---|---|
| `Office::version()` | string | package version |
| `Office::sheet_read($path)` | arrayref of `{name, rows}` | numbers stay numbers, empty cells `undef` |
| `Office::sheet_write($path, $sheets, %opts)` | hashref | `$sheets`: `[{name, rows => [[...]]}]`; `format` opt |
| `Office::doc_read($path)` | list of paragraph strings | docx/odt |
| `Office::doc_write($path, $blocks, %opts)` | hashref | block: `{kind => "para"\|"heading", level, text}` |
| `Office::slides_read($path)` | arrayref of `{text => [...]}` | pptx/odp |
| `Office::slides_write($path, $slides, %opts)` | hashref | slide: `{title, body => [...]}` |
| `Office::pdf_read($path)` | `{pages => [...], text}` | text extraction |
| `Office::pdf_write($path, $lines)` | hashref | `$lines`: arrayref of strings (A4) |

### Rich formatting

Spreadsheet cells and document runs accept styling, not just scalars:

```stryke
# xlsx: a cell can be a scalar OR a rich object
Office::sheet_write("r.xlsx", [{ name => "S", rows => [
    [{ v => "Header", bold => 1, color => "#FF0000", bg => "#FFFF00", align => "center" }],
    [{ v => 42, num_format => "0.00", italic => 1 }],
    [{ f => "=A2*2" }],                       # formula cell
] }])
# cell keys: v|value, f|formula, bold, italic, underline, font, size,
#            color, bg, align (left/center/right), num_format, border

# docx: a block can carry styled runs + alignment
Office::doc_write("r.docx", [
    { kind => "para", align => "center", runs => [
        { text => "Bold ", bold => 1, color => "#0000FF", size => 18 },
        { text => "and italic", italic => 1 },
    ] },
])
# run keys: text, bold, italic, underline, strike, size (pt), color, font, highlight
```

ODF (ods/odt) and PDF are written unstyled — the `lo_odf`/`lo_core` serializers
don't expose per-run styling (a documented crate limitation, not faked).

### Images (handle-based, like a PIL `Image`)

| Function | Returns | Notes |
|---|---|---|
| `Office::img_open($path)` / `img_new($w, $h, %opts)` | `{handle, width, height, mode}` | `color` opt: `"#rrggbb"` or `[r,g,b,a]` |
| `Office::img_save($handle, $path, %opts)` | hashref | format from extension |
| `Office::img_info($handle)` | `{handle, width, height, mode}` | |
| `Office::img_resize($h, $w, $ht, %opts)` / `img_thumbnail($h, $max)` | info | `filter` opt |
| `Office::img_crop($h, $x, $y, $w, $ht)` / `img_rotate($h, $deg)` / `img_flip($h, $dir)` | info / hashref | rotate 90/180/270 exact |
| `Office::img_convert($h, $mode)` | info | `L` / `LA` / `RGB` / `RGBA` |
| `Office::img_paste($h, $src, $x, $y)` | hashref | alpha composite |
| `Office::img_get_pixel($h, $x, $y)` / `img_put_pixel($h, $x, $y, $color)` | `{r,g,b,a}` / hashref | |
| `Office::img_draw_rect / img_draw_line / img_draw_circle / img_draw_text` | hashref | `fill` opt; text uses vendored DejaVu Sans or a `font` path |
| `Office::img_close($handle)` | hashref | release the handle |

## [0x05] No external binaries

Every format is handled by a vendored Rust crate, statically linked into
the cdylib:

| Concern | Crate |
|---|---|
| spreadsheet read (xlsx/xls) | `calamine` |
| spreadsheet read (ods) | native `zip` + `quick-xml` |
| xlsx write | `rust_xlsxwriter` |
| ods / odt / odp write | `lo_odf` (OpenDocument serializers) |
| docx write | `docx-rs` |
| docx / odt / pptx / odp read | native `zip` + `quick-xml` |
| pptx write | native `zip` + hand-built OOXML |
| pdf read + write | `lo_core` (self-contained, no font files) |
| image read + write (all formats) | `image` |
| image drawing (shapes) | `imageproc` |
| image text drawing | `ab_glyph` + vendored `assets/DejaVuSans.ttf` |

There is deliberately no call to `soffice` / LibreOffice. That keeps the
package self-contained and reproducible — `scp` the artifact and it runs.
The trade-off: there is no generic render-conversion (e.g. `docx` → `pdf`
laid out like Word would), which fundamentally needs a layout engine. You
get structured read, structured write, and PDF generation from a content
model.

## [0x06] Tests

```sh
cargo test          # Rust round-trip + FFI-contract tests (write -> read back)
s test t/           # stryke assertion suite (needs the cdylib installed)
```

Every writer is exercised end to end against its matching reader over a
real temp file, so a passing test means the bytes on disk parse back.

## [0x07] Layout

```
stryke-office/
  Cargo.toml             # cdylib crate (format crates, all pure Rust)
  src/lib.rs             # office__* exports + format handlers + tests
  src/pptx_write.rs      # minimal OOXML PowerPoint writer
  src/image_ops.rs       # PIL-style image I/O + manipulation
  stryke.toml            # package manifest + [ffi] table
  lib/Office.stk         # stryke wrapper (use Office)
  assets/DejaVuSans.ttf  # vendored font for image text drawing
  examples/              # spreadsheet, document_and_pdf, presentation, image
  t/                     # stryke assertion suites
  tests/                 # docs/readme/polish lint gates
  docs/                  # GitHub Pages content
  Makefile
```

## [0x08] Roadmap

- Cell styles / formulas on xlsx write; richer docx styling.
- Images and tables in documents and slides.
- Spreadsheet formula evaluation on read (values are already returned).
- xls (legacy binary) write.

## [0xFF] License

MIT. See [LICENSE](LICENSE).
