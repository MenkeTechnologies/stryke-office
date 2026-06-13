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
| Spreadsheet | xlsx, ods, xls, csv, tsv | yes | xlsx, ods, csv, tsv |
| Document | docx, odt, html, md, rtf, txt | yes | docx, odt, html, md, rtf, txt, pdf |
| Presentation | pptx, odp | yes | yes |
| PDF | pdf | text + pages | text |
| Image | png, jpeg, gif, bmp, webp, tiff, avif, ico, tga, qoi, pnm, dds, hdr, exr, ff | yes | yes |

The output format is taken from the path extension; override with
`format => "..."`. Every raster codec the `image` crate ships is enabled, so
read and write cover the full list above.

Images get a full PIL-complete surface: open/new/save/info, resize/thumbnail/
crop/rotate/flip/convert/paste, get/put pixel, ImageDraw-style
rect/line/circle/text, the tone/color filters
(blur/sharpen/brighten/contrast/huerotate/invert/grayscale/gamma/threshold/
posterize/sepia/tint), and a deep processing layer
(autocontrast/equalize/solarize/colorize/emboss/convolve/edges/box_blur/median/
pixelate/vignette/opacity/putalpha/blend/blend_mode/composite/border/trim/
transpose/transverse/histogram/extrema/noise/watermark/split/merge/dilate/
erode). This complements the stryke core `image_*` filter builtins — those
operate on pixel data; this package adds the file I/O and manipulation surface.

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

### Structure (sheets + documents)

```stryke
# xlsx sheet-level structure
Office::sheet_write("s.xlsx", [{
    name => "S",
    rows => [["Title", "x"], [{ link => "https://x.com", v => "site" }, "y"]],
    merges      => [[0,0,0,1]],          # merge A1:B1
    cols        => [{ col => 0, width => 24 }],
    row_heights => [{ row => 0, height => 30 }],
    freeze      => [1, 0],               # freeze top row
    autofilter  => [0,0,2,1],
    table       => [0,0,2,1],            # styled worksheet table
    conditional => [{ range => [1,0,2,1], rule => "greater_than", value => 80,
                      format => { bold => 1, bg => "#C6EFCE" } }],
    validations => [{ range => [1,1,2,1], list => ["yes","no","maybe"] }],  # dropdown
    # page setup + embedded content:
    protect => 1, landscape => 1, tab_color => "#FF8800", zoom => 120,
    header => "&CQ Report", footer => "&Lpage &P", print_area => [0,0,2,1],
    repeat_rows => [0,0], print_gridlines => 1, paper => 9,
    margins => [0.5,0.5,0.6,0.6,0.3,0.3],          # l,r,t,b,header,footer
    notes   => [{ row => 1, col => 0, text => "check", author => "qa" }],
    images  => [{ row => 0, col => 3, path => "logo.png" }],
}], defined_names => [{ name => "Region", formula => "=S!\$A\$1" }])

# read formula strings alongside values
val $sheets = Office::sheet_read("s.xlsx", formulas => 1)
# $sheets->[0]{formulas}[0][2]  -> "A1+B1"

# docx structure: tables, inline images, page breaks, page setup, lists,
# hyperlinks, running header/footer
Office::doc_write("d.docx", [
    { kind => "heading", level => 1, text => "Agenda" },
    { kind => "list", ordered => 1, items => ["First", "Second"] },   # numbered
    { kind => "list", ordered => 0, items => ["a", "b"] },             # bulleted
    { kind => "link", url => "https://x.com", text => "see site" },    # hyperlink
    { kind => "table", rows => [["Name","Qty"], ["Widget","3"]] },
    { kind => "image", path => "logo.png", width => 80, height => 80 },
    { kind => "pagebreak" },
    { kind => "para", text => "Next page" },
], page_size => [11906, 16838], header => "My Report", footer => "confidential")
```

### Charting (data → image handle → any format)

`Office::chart_render` rasterizes a chart and returns an **image handle**, so
you save it in whatever format you want with `img_save` (png/jpeg/gif/bmp/
webp/tiff) or process it further. The classic flow — parse an Excel file,
then render its data as many charts in any format:

```stryke
val $sheets = Office::sheet_read("sales.xlsx")
val @rows   = @{ $sheets->[0]{rows} }
val @cats   = map { $_->[0] } @rows[1..$#rows]
val @sales  = map { $_->[1] } @rows[1..$#rows]

for val $spec ([["bar","png"], ["line","jpg"], ["pie","webp"]]) {
    val ($type, $fmt) = @$spec
    val $c = Office::chart_render($type, [{ name => "Sales", data => \@sales }],
                                  title => "Q sales", categories => \@cats)
    Office::img_save($c->{handle}, "out-$type.$fmt")   # any raster format
    Office::img_close($c->{handle})
}
```

Chart types: `bar`/`column`, `stacked`, `line`, `area`, `step`, `combo`
(per-series `kind => "line"` overlays a line on bars), `scatter` (`data` is
`[[x,y],…]`), `bubble` (`[[x,y,size],…]`), `pie`, `donut`, `histogram` (opt
`bins`), `radar`, `sankey` (`nodes`/`links` instead of series), `waterfall`
(deltas → cumulative), `ohlc`/`candlestick` (`data` is `[[open,high,low,close],…]`),
`boxplot` (raw `data` → min/q1/median/q3/max), `funnel`, `gauge` (`value` +
`max`, no series), `heatmap` (`matrix => [[..],..]` or series-of-rows, no
series required). opts: `title`, `width` (800), `height` (600), `categories`,
per-series `color`, `legend => 0` to suppress, `labels => 1` for data labels,
`x_label`, `y_label`. Every type renders identically in raster **and** SVG.

**Raster and vector output, any format.** Three entry points:
- `chart_render(type, series, %opts)` → raster image handle (then `img_save`
  to png/jpeg/tif/bmp/webp/gif, or process further).
- `chart_svg(type, series, %opts)` → vector **SVG** markup (or write to
  `path =>`).
- `chart_save(type, path, %opts)` → write straight to a file, format by
  extension: `.svg` (vector), `.pdf` (chart embedded in a PDF), or any raster
  extension.

```stryke
Office::chart_save("line", "out.svg", series => $s, categories => \@c)   # vector
Office::chart_save("bar",  "out.pdf", series => $s)                       # pdf
Office::chart_save("pie",  "out.png", series => $s)                       # raster
Office::chart_save("sankey", "flow.svg",
    nodes => [{name=>"A"},{name=>"B"},{name=>"X"}],
    links => [{source=>0,target=>2,value=>5},{source=>1,target=>2,value=>3}])
``` Rendered natively with `imageproc` + the vendored font — no plotters,
no system fonts, no external binaries. (The stryke core also has `*_svg` chart
builtins for quick inline SVG; this renders raster charts you can save/embed
in any format.)

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
| `Office::img_blur / img_sharpen / img_brighten / img_contrast / img_huerotate / img_invert / img_grayscale` | hashref | in-place filters on a handle |
| `Office::img_gamma / img_threshold / img_posterize / img_sepia / img_tint` | hashref | tone / color filters |

#### Extended processing (PIL-complete)

All in-place on a handle unless noted; geometry-changing ops return new
`{handle, width, height, mode}`.

| Function | Notes |
|---|---|
| `img_autocontrast($h, %opts)` | full-range stretch; `cutoff` percent tail clip |
| `img_equalize($h)` | per-channel histogram equalization |
| `img_solarize($h, %opts)` | invert above `threshold` (128) |
| `img_colorize($h, $black, $white)` | map luma to a two-color gradient |
| `img_emboss($h)` / `img_convolve($h, $kernel, %opts)` | fixed emboss / arbitrary 3×3 kernel (`divisor`, `offset`) |
| `img_edges($h, %opts)` | Canny edges → grayscale; `low`/`high` |
| `img_box_blur($h, %opts)` / `img_median($h, %opts)` | box blur (`radius`) / median despeckle (`radius`) |
| `img_pixelate($h, %opts)` | mosaic; `block` |
| `img_vignette($h, %opts)` | radial darkening; `strength` 0..1 |
| `img_opacity($h, $factor)` / `img_putalpha($h, $alpha)` | scale / set alpha |
| `img_blend($h, $src, $alpha)` | cross-fade two handles |
| `img_blend_mode($h, $src, $mode)` | multiply/screen/overlay/darken/lighten/difference/add/subtract |
| `img_composite($h, $src, $mask)` | composite through a grayscale mask |
| `img_border($h, %opts)` / `img_trim($h, %opts)` | add solid border / autocrop uniform border |
| `img_transpose($h)` / `img_transverse($h)` | diagonal / anti-diagonal flip (swaps W/H) |
| `img_histogram($h)` | `{ r, g, b, luma }` 256-bin counts |
| `img_extrema($h)` | per-channel `[min, max]` |
| `img_noise($h, %opts)` | `kind` gaussian/salt_pepper, `amount`, `seed` |
| `img_watermark($h, $text, %opts)` | tiled diagonal watermark; `opacity`, `size`, `gap`, `color`, `font` |
| `img_split($h)` / `img_merge($r, $g, $b, %opts)` | split to channel images / merge back |
| `img_dilate($h, %opts)` / `img_erode($h, %opts)` | morphology; `iterations` |

#### Animation, advanced drawing, transforms, byte I/O

| Function | Notes |
|---|---|
| `img_open_frames($path)` | split animated gif/webp → `{count, frames:[{handle,width,height,delay_ms}]}` |
| `img_save_animated($path, $handles, %opts)` | write animated GIF; `delay`/`delays`/`repeat` |
| `img_montage($handles, %opts)` | grid montage → new handle; `cols`/`gap`/`bg` |
| `img_gradient($h, %opts)` | fill `linear`/`radial` between `from`/`to`; `angle` |
| `img_draw_ellipse($h, $x, $y, $rx, $ry, $color, %opts)` | `fill` opt |
| `img_draw_polygon($h, $points, $color)` | `points` = `[[x,y],…]` |
| `img_draw_text_multiline($h, $x, $y, $text, $color, %opts)` | splits on `\n`; `size`/`line_height`/`font` |
| `img_warp($h, $matrix)` | 3×3 projective (affine/perspective); 9 numbers |
| `img_to_base64($h, %opts)` / `img_from_base64($b64)` | encode/decode (`format` opt); embed images as strings |

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
  src/lib.rs             # office__* exports + spreadsheet/doc/slides/pdf handlers + tests
  src/doc_formats.rs     # csv/tsv + html/md/rtf/txt readers + writers
  src/pptx_write.rs      # minimal OOXML PowerPoint writer
  src/image_ops.rs       # PIL-complete image I/O + manipulation
  src/chart_render.rs    # raster chart renderer (all chart types)
  src/chart_svg.rs       # vector (SVG/PDF) chart renderer
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

- PowerPoint speaker notes + images on slides (notesMaster/notesSlide OOXML).
- Spreadsheet formula *evaluation* on read (formula **strings** already read
  via `formulas => 1`; values are computed by the writing app).
- Per-run styling on ODF (ods/odt) write — blocked on `lo_odf` exposing it.
- xls (legacy binary) write.

## [0xFF] License

MIT. See [LICENSE](LICENSE).
