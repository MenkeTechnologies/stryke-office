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
val $tables = Office::doc_tables("memo.docx")    # recover table grids dropped by doc_read
val $blocks = Office::doc_blocks("memo.docx")    # ordered heading/para/table blocks

# Presentation: build a deck
Office::slides_write("deck.pptx", [
    { title => "Intro", body => ["point one", "point two"] },
])

# PDF: generate and extract text (self-contained, no font files)
Office::pdf_write("out.pdf", ["Line one", "Line two"])
val $info = Office::pdf_read("out.pdf")   # { pages => [...], text => "..." }

# PDF: build a multi-page document — headings, flowing paragraphs, embedded
# images (file or image handle), and vector shapes (base-14 fonts, no embed)
val $chart = Office::chart_render("bar", [{ data => [3, 7] }], categories => ["a", "b"])
Office::pdf_build("report.pdf", [
    { type => "heading", level => 1, text => "Quarterly Report" },
    { type => "paragraph", text => $long_text },              # auto-wrapped/paginated
    { type => "image", handle => $chart->{handle}, width => 300 },
    { type => "pagebreak" },
    { type => "rect", x => 50, y => 80, width => 200, height => 30, color => "#D9E1F2" },
    { type => "text", x => 50, y => 120, text => "footnote", size => 9 },
])
```

## [0x03] Format matrix

| Kind | Formats | Read | Write |
|---|---|---|---|
| Spreadsheet | xlsx, ods, xls, csv, tsv | yes | xlsx, ods, csv, tsv, html, md |
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
| `Office::sheet_read($path, %opts)` | arrayref of `{name, rows}` | numbers stay numbers, empty cells `undef`; `delimiter` for csv/tsv (e.g. `;`) |
| `Office::sheet_write($path, $sheets, %opts)` | hashref | `$sheets`: `[{name, rows => [[...]]}]`; writes xlsx/ods/csv/tsv + html/md tables; `format` opt |
| `Office::sheet_merge($inputs, $output, %opts)` | `{sources, sheets}` | combine workbooks into one; `mode => "rows"` stacks; target ext converts |
| `Office::sheet_union($inputs, $output, %opts)` | `{sources, rows, fields}` | concatenate sheets aligned by column name (SQL UNION) |
| `Office::sheet_stats($path, %opts)` | `{sheet, rows, columns:[{name,count,numeric,blanks,sum?,min?,max?,mean?}]}` | per-column descriptive stats; `sheet`/`header` opts |
| `Office::sheet_describe($path, %opts)` | `{sheet, rows, columns:[{name,count,mean,std,min,p25,p50,p75,max}]}` | pandas-style numeric summary (std + quartiles); `sheet`/`header` opts |
| `Office::sheet_quantile($path, $column, $q, %opts)` | `{column, q, value, count}` | arbitrary percentile of a numeric column (e.g. `q=0.9` for p90) |
| `Office::sheet_agg($path, $column, %opts)` | `{column, agg, value, count}` | single-column scalar aggregate (`agg` sum/mean/min/max/count/median) |
| `Office::sheet_sparkline($path, $column, %opts)` | `{column, sparkline, count, min, max}` | render a numeric column as a Unicode block sparkline string (▁▂▃▄▅▆▇█) |
| `Office::sheet_argmax($path, $column, %opts)` | `{column, row, value, label?}` | locate the row of a column's max/min (pandas `idxmax`/`idxmin`); `min`/`label` opts |
| `Office::sheet_moments($path, $column, %opts)` | `{ok, n, mean, variance, std, skewness, kurtosis}` | distribution moments of a column (skewness g1, excess kurtosis g2); complements `sheet_describe` |
| `Office::sheet_corr($path, %opts)` | `{sheet, columns, matrix}` | correlation matrix between numeric columns (pandas `df.corr()`); `method` pearson (default), spearman (rank), or kendall (tau-b) |
| `Office::sheet_regress($path, $x, $y, %opts)` | `{ok, slope, intercept, r2, n, path?}` | simple OLS regression `lm(y ~ x)`; `output` appends predicted/residual columns, `decimals` rounds coefficients |
| `Office::sheet_ttest($path, $a, $b, %opts)` | `{ok, t, df, p, mean_a, mean_b, n_a, n_b}` | Welch's two-sample t-test between columns; two-sided Student-t p-value; `decimals` opt |
| `Office::sheet_anova($path, %opts)` | `{ok, f, df_between, df_within, p, groups, n}` | one-way ANOVA across group columns (`columns` default all numeric); upper-tail F p-value |
| `Office::sheet_chisq($path, %opts)` | `{ok, chi2, df, p, rows, cols}` | Pearson chi-square test of independence on a contingency table of counts; upper-tail chi-square p-value |
| `Office::sheet_dtypes($path, %opts)` | `{sheet, rows, columns:[{name,type,counts}]}` | infer each column's data type (integer/float/bool/string/mixed/empty; pandas `df.dtypes`) |
| `Office::sheet_mode($path, %opts)` | `{sheet, rows, columns:[{name,mode,count}]}` | most-frequent value (mode) of each column (text or numeric) |
| `Office::sheet_nunique($path, %opts)` | `{sheet, rows, columns:[{name,nunique}]}` | count distinct values per column (cardinality; pandas `nunique`); `dropna` opt |
| `Office::sheet_count($path, %opts)` | `{sheet, rows, columns:[{name,count,blank}]}` | count filled/blank cells per column (completeness; pandas `count`) |
| `Office::sheet_cov($path, %opts)` | `{sheet, columns, matrix}` | sample covariance matrix between numeric columns (diagonal = sample variance; pandas `df.cov()`) |
| `Office::sheet_find($path, $query, %opts)` | `{count, matches:[{sheet,row,col,ref,value}]}` | locate cells (A1 refs); `ignore_case`/`whole`/`sheet` opts |
| `Office::sheet_records($path, %opts)` | `{fields, count, records:[{field=>value}]}` | read a sheet as header-keyed objects |
| `Office::sheet_to_map($path, $key, $value, %opts)` | `{map, count}` | build a `{key=>value}` hash from two columns (lookup/config map); last wins on dup keys |
| `Office::records_write($path, $records, %opts)` | `{rows, fields}` | write objects to a sheet (header + rows); `fields`/`sheet_name` opts |
| `Office::sheet_to_json($path, $output, %opts)` | `{count}` | export a sheet to a JSON file (array of objects); `pretty` opt |
| `Office::sheet_to_ndjson($path, $output, %opts)` | `{ok, path, count}` | export a sheet as JSON Lines/NDJSON (one object per data row); `sheet` opt |
| `Office::ndjson_to_sheet($output, %opts)` | `{ok, path, rows, fields}` | import JSON Lines/NDJSON into a spreadsheet; `input`/`ndjson`/`fields` opts |
| `Office::json_to_sheet($input, $output, %opts)` | `{rows, fields}` | import a JSON array-of-objects file into a spreadsheet |
| `Office::sheet_sort($path, $by, $output, %opts)` | `{sorted, column}` | sort data rows by a column (header kept); `descending`/`numeric`/`ignore_case` opts |
| `Office::sheet_multisort($path, $keys, $output, %opts)` | `{ok, path, sorted}` | sort by multiple columns; `keys` = `[{column, descending?}]` in priority order |
| `Office::sheet_rank($path, $by, $output, %opts)` | `{ok, path, ranked}` | append a rank column by a column without reordering (Excel `RANK`); `ascending`/`dense`/`name` opts |
| `Office::sheet_pct_rank($path, $column, $output, %opts)` | `{ok, path, column}` | append a percentile-rank column (empirical CDF, 0..1; pandas `rank(pct=True)`); `into`/`decimals` opts |
| `Office::sheet_freq($path, $column, %opts)` | `{column, total, distinct, values}` | value-counts of a column sorted by frequency (pandas `value_counts`); `ignore_case`/`top` opts |
| `Office::sheet_unique($path, $column, %opts)` | `{column, count, values}` | distinct values of a column (SQL `DISTINCT`); `sorted`/`ignore_case` opts |
| `Office::sheet_group_concat($path, $group_by, $value, $output, %opts)` | `{ok, path, groups}` | group by a column, concatenate another's values (SQL `GROUP_CONCAT`); `sep`/`distinct` opts |
| `Office::sheet_explode($path, $column, $output, %opts)` | `{ok, path, rows}` | split a delimited column into multiple rows (SQL `unnest`); `sep`/`trim` opts |
| `Office::sheet_map($path, $column, $mapping, $output, %opts)` | `{ok, path, mapped}` | recode a column's values via a lookup map; `default`/`into` opts |
| `Office::sheet_partition($path, $column, $dir, %opts)` | `{count, files}` | split a sheet into one file per distinct value of a column; `prefix`/`format` opts |
| `Office::sheet_split_by($path, $output, $column, %opts)` | `{ok, path, sheets, groups}` | split a sheet into one tab per distinct value of a column, in a single workbook; `header` opt |
| `Office::sheet_lookup($path, $lookup, $key, $result, %opts)` | `{found, value, row}` | VLOOKUP — find a key in one column, return the cell from another; `ignore_case` opt |
| `Office::sheet_countif($path, $column, %opts)` | `{count}` | count rows where a column matches a predicate (COUNTIF); `op`/`value`/`ignore_case` opts |
| `Office::sheet_sumif($path, $column, %opts)` | `{sum, count}` | sum a column over rows matching a predicate (SUMIF); `op`/`value`/`sum` opts |
| `Office::sheet_split_column($path, $column, $output, %opts)` | `{ok, path, columns}` | split a column into several by a delimiter (Text to Columns); `delimiter`/`into`/`max`/`keep` opts |
| `Office::sheet_concat_columns($path, $columns, $output, %opts)` | `{ok, path, into}` | join several columns into one with a separator (Excel `TEXTJOIN`); `separator`/`into`/`skip_blanks`/`keep` opts |
| `Office::sheet_filter($path, $by, $output, %opts)` | `{kept, removed, column}` | keep rows matching `op` (eq/ne/contains/gt/lt/ge/le) on a column |
| `Office::sheet_flag($path, $by, $output, %opts)` | `{ok, path, column, flagged}` | append a flag column marking rows matching a predicate (keeps all rows); `op`/`value`/`true_value`/`false_value`/`into` opts |
| `Office::sheet_onehot($path, $output, $column, %opts)` | `{ok, path, categories}` | one-hot encode a categorical column into 0/1 indicator columns (pandas `get_dummies`); `prefix`/`drop` opts |
| `Office::sheet_where($path, $conditions, $output, %opts)` | `{ok, path, kept}` | keep rows matching multiple conditions (`[{column,op,value}]`); `match` all/any |
| `Office::sheet_freeze($path, $output, %opts)` | `{ok, path, row, col}` | freeze panes on a sheet (xlsx); `row` (default 1)/`col` opts |
| `Office::sheet_autofilter($path, $output, %opts)` | `{ok, path, range}` | apply an autofilter over a sheet's range (xlsx); `range` opt (default used range) |
| `Office::sheet_merge_cells($path, $ranges, %opts)` | `{ok, path, merged}` | merge cell ranges (xlsx); `ranges` of `[r1,c1,r2,c2]`; top-left value fills each |
| `Office::sheet_autosize($path, %opts)` | `{ok, path, sheets}` | auto-size columns to content (xlsx); `sheet` opt limits to one sheet (default all) |
| `Office::sheet_protect($path, %opts)` | `{ok, path, sheets}` | enable worksheet protection (xlsx); `password`/`sheet` opts |
| `Office::sheet_comments($path)` | `{comments:[{cell,author,text}], count}` | extract cell comments/notes from an xlsx (`xl/comments*.xml`) |
| `Office::sheet_cast($path, $output, %opts)` | `{ok, path, cast}` | type-coerce column(s) (`number`/`int`/`string`/`bool`); `number` parses currency/commas/percent/accounting negatives; `by` opt |
| `Office::sheet_strip($path, %opts)` | `{ok, path, trimmed}` | trim whitespace from every string cell (whole-sheet); `collapse` squeezes internal runs |
| `Office::sheet_pad($path, $output, $column, $width, %opts)` | `{ok, path, padded}` | pad a column's values to a fixed width (e.g. zero-pad IDs); `fill`/`side`/`into` opts |
| `Office::sheet_substr($path, $output, $column, %opts)` | `{ok, path, column}` | extract a fixed-position substring from a column (SQL `SUBSTRING`); `start`/`len`/`into` opts |
| `Office::sheet_extract($path, $output, $column, $pattern, %opts)` | `{ok, path, column, matched}` | extract a regex match/capture group into a new column (pandas `str.extract`); `group`/`into` opts |
| `Office::sheet_grep($path, $output, $pattern, %opts)` | `{ok, path, kept, removed}` | keep rows whose cell matches a regex; `column` (default any cell)/`invert`/`ignore_case` opts |
| `Office::sheet_reverse($path, %opts)` | `{ok, path, rows}` | reverse data-row order (header kept on top) |
| `Office::sheet_coalesce($path, $output, $columns, %opts)` | `{ok, path, column, filled}` | append a column with the first non-blank value across columns (SQL `COALESCE`); `into`/`default` opts |
| `Office::sheet_recode($path, $output, $column, $map, %opts)` | `{ok, path, recoded}` | remap a column's values via a `{old=>new}` dict (pandas `Series.map`); `default`/`into` opts |
| `Office::sheet_duplicates($path, %opts)` | `{duplicates, groups:[{key,count,rows}]}` | find/report duplicate rows by key (data-quality audit; counterpart to `dedupe`); `by` opt |
| `Office::sheet_round($path, $output, %opts)` | `{ok, path, rounded}` | round every numeric cell to N decimals (`decimals` default 2); `columns` opt restricts which columns; header untouched |
| `Office::sheet_histogram($path, $column, %opts)` | `{column, count, min, max, bins}` | bucket a numeric column into `bins` (default 10) equal-width intervals; each bin `{lo, hi, count}` |
| `Office::sheet_bin($path, $output, $column, %opts)` | `{ok, path, column, into, bins}` | append a bin-assignment column (pandas `pd.cut`); `edges`/`bins`/`labels`/`into` opts |
| `Office::sheet_ntile($path, $output, $column, %opts)` | `{ok, path, column, into, buckets}` | append an equal-frequency bucket column (SQL `NTILE` / pandas `qcut`); `n`/`labels`/`into` opts |
| `Office::sheet_outliers($path, $column, %opts)` | `{column, method, count, lower, upper, outliers}` | detect outlier rows via `iqr` Tukey fence (default, `k` 1.5) or `zscore` (`k` 3); each outlier `{row, value}` |
| `Office::sheet_aggregate($path, $group_by, $output, %opts)` | `{groups}` | SQL-style GROUP BY; `agg` count/sum/mean/min/max over a `value` column |
| `Office::sheet_pivot($path, $rows, $cols, $output, %opts)` | `{rows, cols}` | pivot table (rows × cols → aggregated `value`); Excel PivotTable; `margins` adds row/col totals (count/sum) |
| `Office::sheet_unpivot($path, $output, %opts)` | `{rows}` | melt wide→long; `id_vars`/`value_vars`/`var_name`/`value_name` |
| `Office::sheet_join($left, $right, $output, %opts)` | `{rows, matched}` | SQL JOIN two sheets on a key; `on`/`left_on`/`right_on`, `how` inner/left/right/outer |
| `Office::sheet_select($path, $columns, $output, %opts)` | `{columns}` | project/reorder columns by name or index |
| `Office::sheet_drop($path, $columns, $output, %opts)` | `{columns}` | remove columns (complement of sheet_select) |
| `Office::sheet_add_column($path, $name, %opts)` | `{column}` | add a derived column: `value` constant or `concat` of columns |
| `Office::sheet_totals($path, %opts)` | `{totals}` | append a totals row summing each numeric column; `label` opt |
| `Office::sheet_subtotal($path, %opts)` | `{ok, path, groups}` | insert group-wise subtotal rows (Excel Data▸Subtotal); `group`/`value`/`agg`/`label`/`grand` opts (sort by group first) |
| `Office::sheet_replace($path, $find, %opts)` | `{replaced}` | find/replace cell text (any format incl. csv); `ignore_case`/`whole`/`column` |
| `Office::sheet_transpose($path, $output, %opts)` | `{rows, columns}` | swap rows and columns |
| `Office::sheet_dedupe($path, $output, %opts)` | `{kept, removed}` | drop duplicate rows; `by` key columns, `keep` first/last |
| `Office::sheet_append($path, %opts)` | `{added, rows}` | append `rows` or header-mapped `records` to a sheet (in place by default) |
| `Office::sheet_hstack($path, $right, $output, %opts)` | `{ok, path, rows, columns}` | concatenate two row-aligned sheets side by side (pandas `concat axis=1`); `sheet`/`right_sheet` opts |
| `Office::sheet_cross($path, $right, $output, %opts)` | `{ok, path, rows}` | cartesian product (cross join) of two sheets (pandas merge `how='cross'`); `sheet`/`right_sheet` opts |
| `Office::sheet_fill($path, %opts)` | `{filled}` | fill blank cells; `method` ffill/bfill/value, `by` columns, `value` constant |
| `Office::sheet_impute($path, %opts)` | `{ok, path, filled, columns}` | fill blanks with a column statistic (sklearn `SimpleImputer`); `strategy` mean/median/mode/zero, `by`/`decimals` opts |
| `Office::sheet_interpolate($path, %opts)` | `{ok, path, filled}` | fill internal blanks in numeric columns by linear interpolation (pandas `Series.interpolate`); `by`/`decimals` opts |
| `Office::sheet_drop_empty($path, $output, %opts)` | `{ok, path, rows_removed, cols_removed}` | drop fully-empty rows and/or columns; `rows`/`cols` opts |
| `Office::sheet_dropna($path, %opts)` | `{ok, path, kept, removed}` | drop rows blank in specified column(s) (pandas `dropna(subset)`); `by`/`how` (any/all) opts |
| `Office::sheet_add_header($path, $names, $output, %opts)` | `{ok, path, columns}` | prepend a header row of column names (for headerless data) |
| `Office::sheet_add_index($path, $output, %opts)` | `{ok, path, rows}` | prepend a sequential row-number column (row IDs); `name`/`start`/`step` opts |
| `Office::sheet_calc($path, $left, $op, $output, %opts)` | `{ok, path, column}` | append a computed column (`+ - * / %`) between two columns or a column and `value`; `into` required |
| `Office::sheet_row_stats($path, $columns, $output, %opts)` | `{ok, path, column}` | row-wise reduction across `columns` into a new column (pandas `df[cols].agg(axis=1)`); `agg` sum/mean/min/max/count/product/range |
| `Office::sheet_split($path, $dir, %opts)` | `{count, files}` | explode a workbook into one file per sheet; `format`/`prefix` opts |
| `Office::sheet_chunk($path, $size, $dir, %opts)` | `{count, files}` | split rows into fixed-size chunks across files (header repeated) |
| `Office::sheet_head($path, $output, %opts)` | `{rows}` | keep first (or `tail`) N data rows; preview large data |
| `Office::sheet_top($path, $by, $output, %opts)` | `{rows}` | top-N rows by a column (sort + limit); `n`/`ascending` |
| `Office::sheet_sample($path, $output, %opts)` | `{ok, path, rows}` | randomly sample N data rows (header kept), reproducible via `seed`; `n` opt |
| `Office::sheet_stratified_sample($path, $group, $output, %opts)` | `{ok, path, rows, groups}` | stratified sample preserving each group's row share (sklearn stratify); `ratio` or `n_per_group`, `seed` opts |
| `Office::sheet_shuffle($path, %opts)` | `{ok, path, rows}` | reproducibly shuffle data rows (seeded Fisher–Yates); `seed` opt |
| `Office::sheet_train_test_split($path, $train, $test, %opts)` | `{ok, train, test, train_rows, test_rows}` | split rows into train/test files (sklearn `train_test_split`); `ratio`/`shuffle`/`seed` opts |
| `Office::sheet_transform($path, $column, $op, $output, %opts)` | `{ok, path, transformed}` | apply a per-column op (upper/lower/trim/title/round/floor/ceil/abs/int); `digits`/`into` opts |
| `Office::sheet_rename($path, $to, %opts)` | `{renamed}` | rename a sheet in a workbook; `from` selects which |
| `Office::sheet_add($path, $name, %opts)` | `{sheets}` | add a new sheet to a workbook; `rows`/`position` opts |
| `Office::sheet_copy($path, %opts)` | `{ok, path, name, sheets}` | duplicate a worksheet within a workbook; `sheet`/`name`/`position` opts |
| `Office::sheet_remove($path, $sheet, %opts)` | `{removed, sheets}` | remove a sheet from a workbook (by name/index) |
| `Office::sheet_reorder($path, $order, %opts)` | `{sheets}` | reorder/subset workbook sheets by an `order` list |
| `Office::info($path)` | `{type, format, …}` | universal inspector: identify any office/image file + type summary |
| `Office::sheet_info($path)` | `{count, sheets:[{name,rows,cols}]}` | workbook overview: sheet names + dimensions |
| `Office::sheet_diff($left, $right, %opts)` | `{count, changed:[{ref,row,col,left,right}], left_rows, right_rows}` | cell-by-cell diff of two sheets |
| `Office::sheet_to_slides($path, $output, %opts)` | `{slides}` | one slide per row; `title_field` titles each, other fields → body lines |
| `Office::sheet_validate($path, $rules, %opts)` | `{valid, count, violations:[{ref,column,rule,value}]}` | per-column rules: type/min/max/allowed/nonempty |
| `Office::doc_read($path)` | list of paragraph strings | docx/odt |
| `Office::doc_tables($path)` | `{tables:[{rows:[[cell,…]]}], count}` | extract every table as a string grid (docx/odt) |
| `Office::doc_table_to_sheet($path, $output, %opts)` | `{ok, path, rows, cols}` | extract one doc table into a spreadsheet file (xlsx/ods/csv); `index`/`name` opts |
| `Office::doc_to_sheet($path, $output, %opts)` | `{ok, path, rows}` | extract a document into a spreadsheet (one row per block: level, text) |
| `Office::sheet_to_doc($path, $output, %opts)` | `{ok, path, rows, cols}` | render a spreadsheet as a table inside a docx/odt; `sheet`/`title` opts |
| `Office::sheet_to_md($path, %opts)` | `{ok, rows, cols, markdown, path?}` | render a spreadsheet as a GitHub-flavored Markdown table; `output`/`sheet`/`header` opts |
| `Office::sheet_to_sql($path, $table, %opts)` | `{ok, statements, rows, sql, path?}` | emit SQL `INSERT` statements for a sheet; `columns`/`batch`/`output` opts |
| `Office::sheet_to_latex($path, %opts)` | `{ok, rows, cols, latex, path?}` | render a sheet as a LaTeX `tabular`; `align`/`booktabs`/`caption`/`output` opts |
| `Office::sheet_to_csv($path, %opts)` | `{ok, rows, csv, path?}` | serialize a sheet as an RFC-4180 CSV string (proper quoting); `delimiter`/`output` opts |
| `Office::csv_to_sheet($output, %opts)` | `{ok, path, rows, cols}` | parse RFC-4180 CSV text/file into a sheet (inverse of `sheet_to_csv`); `csv`/`input`/`delimiter`/`numeric` opts |
| `Office::md_to_sheet($output, %opts)` | `{ok, path, rows, cols}` | parse a Markdown table into a spreadsheet file; `markdown`/`path`/`name` opts |
| `Office::html_to_sheet($input, $output, %opts)` | `{ok, path, rows, cols}` | parse an HTML table into a spreadsheet (web scraping); `index`/`name` opts |
| `Office::sheet_to_html($path, %opts)` | `{ok, rows, cols, html, path?}` | render a spreadsheet as an HTML table (thead/tbody, escaped); `output`/`title`/`full` opts |
| `Office::sheet_to_text($path, %opts)` | `{ok, rows, cols, text, path?}` | render a spreadsheet as an aligned plain-text table; `border`/`output`/`header` opts |
| `Office::sheet_get_cell($path, $cell, %opts)` | `{cell, row, col, value}` | read a single cell by A1 reference (e.g. "B2"); `sheet` opt |
| `Office::sheet_set_cell($path, $cell, %opts)` | `{ok, path, cell}` | set a single cell by A1 reference (grows the grid); `value`/`output`/`sheet` opts |
| `Office::sheet_get_range($path, $range, %opts)` | `{range, nrows, ncols, rows}` | read a rectangular A1 range (e.g. "A1:C3") as a subgrid; `sheet` opt |
| `Office::sheet_set_range($path, $cell, $values, %opts)` | `{ok, path, cells}` | paste a 2D block at a top-left A1 cell (grows the grid); `output`/`sheet` opts |
| `Office::sheet_insert_rows($path, %opts)` | `{ok, path, inserted}` | insert blank rows at a 1-based position (shifts down); `at`/`count`/`sheet` opts |
| `Office::sheet_delete_rows($path, $at, %opts)` | `{ok, path, deleted}` | delete rows from a 1-based position; `count`/`output`/`sheet` opts |
| `Office::sheet_insert_column($path, %opts)` | `{ok, path, at}` | insert a column at a 1-based position (shifts right); `at`/`name`/`value` opts |
| `Office::sheet_cumsum($path, $column, $output, %opts)` | `{ok, path, column}` | append a running-total (cumulative sum) column for a numeric column; `into` opt |
| `Office::sheet_cumulative($path, $column, $output, %opts)` | `{ok, path, column}` | append a cumulative running max/min/product column (pandas `cummax`/`cummin`/`cumprod`); `agg`/`into`/`decimals` opts |
| `Office::sheet_pct($path, $column, $output, %opts)` | `{ok, path, column}` | append a percent-of-total column (value ÷ column sum × 100); `into`/`decimals` opts |
| `Office::sheet_group_pct($path, $output, $group, $value, %opts)` | `{ok, path, column}` | append a percent-of-group-total column (value ÷ group sum × 100); `into`/`decimals` opts |
| `Office::sheet_resample($path, $date, $output, %opts)` | `{ok, path, buckets}` | roll up an ISO-date column into day/month/year buckets and aggregate; `freq`/`value`/`agg` opts |
| `Office::sheet_group_stats($path, $group, $value, $output, %opts)` | `{ok, path, groups}` | per-group `[group, count, mean, std, min, max]` for a numeric column (pandas `groupby.describe`; sample std) |
| `Office::sheet_date_part($path, $output, $column, %opts)` | `{ok, path, column}` | extract year/month/day/ym from an ISO-date column into a new column; `part`/`into` opts |
| `Office::sheet_standardize($path, $output, %opts)` | `{ok, path, columns}` | z-score numeric column(s) in place (whole-sheet); `by`/`decimals` opts |
| `Office::sheet_running($path, $output, $group, $value, %opts)` | `{ok, path, column}` | append a group-wise running total column (running balance per group; pandas `groupby.cumsum`); `into`/`decimals` opts |
| `Office::sheet_normalize($path, $column, $output, %opts)` | `{ok, path, column}` | append a normalized column (`minmax` 0..1 or `zscore`); `method`/`into`/`decimals` opts |
| `Office::sheet_movavg($path, $column, $window, $output, %opts)` | `{ok, path, column}` | append a moving-average (rolling mean) column over a window; `into`/`decimals` opts |
| `Office::sheet_rolling($path, $column, $window, $output, %opts)` | `{ok, path, column}` | append a rolling-window aggregate column; `agg` sum/mean/min/max/median/std (superset of `sheet_movavg`) |
| `Office::sheet_ewm($path, $column, $output, %opts)` | `{ok, path, column}` | append an exponentially-weighted moving average (pandas `ewm().mean()`); `alpha` or `span`, `into`/`decimals` opts |
| `Office::sheet_delta($path, $column, $output, %opts)` | `{ok, path, column}` | append a row-over-row difference (current − previous) column; `into`/`decimals` opts |
| `Office::sheet_pct_change($path, $column, $output, %opts)` | `{ok, path, column}` | append a row-over-row percentage-change column (pandas `pct_change`); `fraction`/`into`/`decimals` opts |
| `Office::sheet_shift($path, $column, $output, %opts)` | `{ok, path, column}` | append a shifted (lag/lead) copy of a column (pandas `Series.shift`); `periods`/`fill`/`into` opts |
| `Office::sheet_clamp($path, $column, $output, %opts)` | `{ok, path, clamped}` | clamp a numeric column to a range (cap outliers); `min`/`max`/`into` opts |
| `Office::sheet_winsorize($path, $column, $output, %opts)` | `{ok, path, clipped, low, high}` | clip a numeric column to percentile bounds (robust outlier capping); `lower`/`upper`/`into`/`decimals` opts |
| `Office::sheet_rename_column($path, $column, $to, $output, %opts)` | `{ok, path, column}` | rename a column's header (not the sheet tab); `sheet`/`format` opts |
| `Office::sheet_rename_columns($path, $output, $map, %opts)` | `{ok, path, renamed}` | bulk-rename headers via a `{old=>new}` map |
| `Office::doc_blocks($path)` | `{blocks:[{kind,…}], count}` | ordered structural read: heading/para/table in document order (docx/odt) |
| `Office::doc_outline($path)` | `{outline:[{level,text}], count}` | heading outline of a docx/odt (document analogue of pdf_outline) |
| `Office::doc_links($path)` | `{links:[{text,url}], count}` | extract hyperlinks (docx via rels, odt `text:a`); internal links → `#anchor` |
| `Office::doc_stats($path)` | `{words, characters, characters_no_spaces, lines, paragraphs, pages?}` | Word-style counts across docx/odt/html/md/rtf/txt/pdf |
| `Office::doc_wordfreq($path, %opts)` | `{total, unique, words:[{word,count}]}` | word-frequency ranking; `top`/`min_length`/`stopwords` opts |
| `Office::doc_readability($path)` | `{words, sentences, syllables, flesch_reading_ease, flesch_kincaid_grade}` | Flesch Reading Ease + Flesch–Kincaid Grade Level (heuristic syllable count) |
| `Office::doc_sentences($path, %opts)` | `{count, sentences}` | split a document's text into sentences (NLP prep); `max` opt |
| `Office::doc_diff($a, $b)` | `{same, added, removed, added_paragraphs, removed_paragraphs}` | order-aware LCS paragraph diff between two documents |
| `Office::doc_comments($path)` | `{comments:[{id,author,date,initials,text}], count}` | extract review comments from a docx (`word/comments.xml`) |
| `Office::doc_footnotes($path, %opts)` | `{notes:[{id,text}], count}` | extract footnotes (or `endnotes`) from a docx (`word/footnotes.xml`) |
| `Office::doc_merge($inputs, $output, %opts)` | `{sources, blocks}` | concatenate documents into one; target ext converts too; `page_breaks` toggle |
| `Office::doc_append($path, $blocks, %opts)` | `{blocks, added}` | append blocks to an existing document (in place by default); `page_break` toggle |
| `Office::doc_split($path, $dir, %opts)` | `{count, files}` | split a document into files at headings; `level`/`format`/`prefix` opts |
| `Office::md_to_doc($input, $output, %opts)` | `{blocks}` | convert Markdown (headings/lists/tables) to docx/odt/pdf/html by output ext |
| `Office::html_to_doc($input, $output, %opts)` | `{blocks}` | convert HTML (h1-6/p/lists/tables) to docx/odt/pdf/md by output ext |
| `Office::doc_to_md($path, $output)` | `{blocks}` | convert a docx/odt to structured Markdown (headings + tables); inverse of md_to_doc |
| `Office::doc_to_html($path, $output)` | `{blocks}` | convert a docx/odt to structured HTML; inverse of html_to_doc |
| `Office::doc_to_text($path, $output)` | `{chars}` | extract any readable document's plain text to a file (incl. pdf) |
| `Office::doc_to_pdf($path, $output)` | `{ok, path, elements}` | render a docx/odt document to a PDF (headings/paras/lists/tables) |
| `Office::doc_add_toc($path, %opts)` | `{ok, path, entries}` | generate a Table of Contents from headings and prepend it; `title`/`pagebreak` opts |
| `Office::html_to_pdf($input, $output)` | `{ok, path, elements}` | render an HTML file straight to a PDF (headings/paras/lists/tables) |
| `Office::md_to_pdf($input, $output)` | `{ok, path, elements}` | render a Markdown file straight to a PDF (headings/lists/pipe tables) |
| `Office::pdf_to_doc($path, $output, %opts)` | `{ok, path, pages, paragraphs}` | convert a PDF's text into a docx/odt (or md/html/txt) doc with page breaks |
| `Office::pdf_to_slides($path, $output, %opts)` | `{ok, path, slides}` | convert a PDF into a deck, one slide per page (first line→title) |
| `Office::pdf_to_sheet($path, $output, %opts)` | `{ok, path, rows}` | extract a PDF's text into a spreadsheet (one row per line: page, text) |
| `Office::doc_to_slides($path, $output, %opts)` | `{slides}` | turn a document into a deck — headings become slide titles, content becomes bullets |
| `Office::slides_to_doc($path, $output, %opts)` | `{slides}` | turn a deck into a document (slides → headings + paragraphs); `notes` opt |
| `Office::slides_to_pdf($path, $output, %opts)` | `{slides}` | render a deck to a PDF handout, one slide per page; `notes` opt |
| `Office::slides_outline($path)` | `{count, outline}` | extract slide titles as a deck outline/TOC (`[{slide, title}]`) |
| `Office::slides_to_md($path, %opts)` | `{ok, slides, markdown, path?}` | render a deck as a Markdown outline (titles→headings, body→bullets); `output`/`level`/`notes` opts |
| `Office::slides_to_html($path, %opts)` | `{ok, slides, html, path?}` | render a deck as an HTML page (section per slide, `<h2>`+`<ul>`); `output`/`notes`/`full` opts |
| `Office::slides_to_text($path, %opts)` | `{ok, slides, text, path?}` | extract a deck's text as plain text, slide by slide; `output`/`notes`/`sep` opts |
| `Office::slides_to_sheet($path, $output, %opts)` | `{ok, path, slides}` | extract a deck into a spreadsheet (number/title/text per slide); `sep` opt |
| `Office::md_to_slides($output, %opts)` | `{ok, path, slides}` | parse a Markdown outline into a deck (headings→slides, bullets→body); `markdown`/`path` opts |
| `Office::slides_split($path, $dir, %opts)` | `{count, files}` | split a deck into one file per slide; `prefix`/`format` opts |
| `Office::slides_reorder($path, $order, %opts)` | `{ok, path, slides}` | reorder/subset slides by a 1-based order list (deck analogue of `pdf_reorder`) |
| `Office::slides_delete($path, $slides, %opts)` | `{ok, path, removed, slides}` | delete slides by 1-based number/array (deck analogue of `pdf_delete`); refuses to empty the deck |
| `Office::slides_insert($path, %opts)` | `{ok, path, position, slides}` | insert a new slide at a 1-based position (default append); `title`/`body`/`position` opts |
| `Office::slides_set_title($path, $slide, $title, %opts)` | `{ok, path, slide}` | set/replace a slide's title (body preserved) |
| `Office::slides_set_body($path, $slide, $body, %opts)` | `{ok, path, slide}` | set/replace a slide's body lines (title preserved) |
| `Office::doc_find($path, $query, %opts)` | `{count, matches:[{paragraph,count,snippet}]}` | search document paragraphs (docx/odt/html/md/rtf/txt/pdf); `regex`/`ignore_case` opts |
| `Office::text_grep($path, $query, %opts)` | `{count, matches:[{line, text}]}` | grep matching lines from a text file; `regex`/`ignore_case`/`invert`/`max` opts |
| `Office::text_stats($path)` | `{lines, words, chars, bytes}` | `wc`-style stats for a raw text file |
| `Office::text_sort($path, %opts)` | `{ok, path, lines}` | sort a text file's lines (`sort`/`uniq`); `descending`/`numeric`/`unique`/`ignore_case` opts |
| `Office::text_uniq($path, %opts)` | `{ok, path, lines}` | collapse duplicate lines (`uniq`); `count`/`global`/`ignore_case` opts |
| `Office::text_sed($path, $pattern, $replacement, %opts)` | `{ok, path, replaced}` | regex find/replace over a text file (`sed s/…/…/g`) with `$1` backreferences; `global`/`ignore_case` opts |
| `Office::text_extract($path, $pattern, %opts)` | `{count, matches, path?}` | extract all regex matches/capture groups from a text file into a list; `group`/`unique`/`ignore_case`/`output` opts |
| `Office::text_cut($path, $fields, %opts)` | `{count, lines, path?}` | extract delimited fields per line (`cut -d -f`); `delim`/`output_delim`/`output` opts |
| `Office::text_wrap($path, %opts)` | `{ok, path, lines}` | wrap long lines to a max width (`fmt`/`fold -s`); `width`/`break_words`/`output` opts |
| `Office::text_tr($path, $from, %opts)` | `{ok, path}` | translate/delete/squeeze characters (`tr`); `a-z`/`0-9` ranges, `to`/`delete`/`squeeze`/`complement` opts |
| `Office::text_paste($paths, %opts)` | `{count, lines, path?}` | merge lines of several files side by side (`paste`); `delim`/`output` opts, short files padded |
| `Office::text_comm($a, $b, %opts)` | `{only_a, only_b, both, a_count, b_count, common}` | set-based line comparison of two files (`comm`); `ignore_case` opt |
| `Office::text_join($a, $b, %opts)` | `{count, lines, path?}` | relational inner join of two delimited files on a key `field` (`join`); `delim`/`output` opts |
| `Office::text_head($path, %opts)` | `{count, lines, path?}` | first (or last) N lines of a text file (`head`/`tail`); `n`/`tail`/`output` opts |
| `Office::slides_find($path, $query, %opts)` | `{count, matches:[{slide,where,value}]}` | search slide text + speaker notes (pptx/odp) |
| `Office::doc_write($path, $blocks, %opts)` | hashref | block: `{kind => "para"\|"heading", level, text}`; opts `header`/`footer`/`page_numbers`/`page_size` |
| `Office::slides_read($path)` | arrayref of `{text => [...], notes => [...]}` | pptx/odp; `notes` = speaker notes |
| `Office::slides_write($path, $slides, %opts)` | hashref | slide: `{title, body => [...]}` |
| `Office::slides_add_image($path, $image, %opts)` | `{ok, path, slide, image}` | embed a picture onto a pptx slide; `slide`/`x`/`y`/`width`/`height` (px) opts |
| `Office::slides_add_text($path, $text, %opts)` | `{ok, path, slide}` | add a text box to a pptx slide; `slide`/`x`/`y`/`width`/`height`/`size` opts |
| `Office::slides_set_notes($path, $notes, %opts)` | `{ok, path, slide, lines}` | set/replace a slide's speaker notes (string or lines); `slide`/`output` opts |
| `Office::slides_merge($inputs, $output, %opts)` | `{sources, slides}` | concatenate decks into one (pptx/odp); target ext converts |
| `Office::slides_stats($path)` | `{slides, words, notes_words, per_slide:[{words,notes_words}]}` | per-deck statistics |
| `Office::slides_append($path, $slides, %opts)` | `{slides, added}` | append slides to an existing deck (in place by default) |
| `Office::pdf_read($path)` | `{pages => [...], text}` | text extraction |
| `Office::pdf_write($path, $lines)` | hashref | `$lines`: arrayref of strings (A4) |
| `Office::pdf_build($path, $elements, %opts)` | `{pages, bytes}` | multi-page: heading/paragraph/text/image/rect/line/table/pagebreak; `page_size`/`margin` |
| `Office::images_to_pdf($images, $output, %opts)` | `{pages}` | combine image files into a PDF, one per page, fit-to-page; `page_size`/`margin` |
| `Office::sheet_to_pdf($path, $output, %opts)` | `{pages, bytes}` | render a spreadsheet as a bordered PDF table; `title`/`header`/`landscape` opts |
| `Office::pdf_merge($inputs, $path)` | `{pages, merged}` | concatenate PDFs (input order) |
| `Office::pdf_blank($output, %opts)` | `{ok, path, pages, width, height}` | generate a blank N-page PDF; `pages`/`size` (a4/letter/legal/a3/a5)/`width`/`height` opts |
| `Office::pdf_split($path, $pages, $output)` | `{pages}` | extract 1-based page subset to a new PDF |
| `Office::pdf_rotate($path, $angle, $output, %opts)` | `{rotated, angle}` | rotate pages 90°-multiples; `pages` subset |
| `Office::pdf_info($path)` | `{pages, version, width, height, mediabox, cropbox?, title?, …}` | page count, first-page geometry + document metadata |
| `Office::pdf_page_sizes($path)` | `{pages, count}` | per-page dimensions in points (`[{page, width, height}]`) |
| `Office::pdf_crop($path, $output, %opts)` | `{cropped}` | set the crop box; `box`=[x0,y0,x1,y1] or `margins`=[l,b,r,t]; `pages` subset |
| `Office::pdf_watermark($path, $text, $output, %opts)` | `{stamped}` | rotated text watermark on every page; `size`/`color`/`angle` |
| `Office::pdf_page_numbers($path, $output, %opts)` | `{pages}` | footer page numbers; `format` (`{n}`/`{total}`), `size`/`color`/`y` |
| `Office::pdf_encrypt($path, $output, %opts)` | `{method}` | password-protect; `owner_password`/`user_password`, `aes` (AES-128 vs RC4), `key_length`, `permissions` |
| `Office::pdf_decrypt($path, $output, %opts)` | `{path}` | strip protection given `password` (owner or user) |
| `Office::pdf_compress($path, $output)` | `{before, after, saved}` | prune unused objects + deflate streams; reports byte savings |
| `Office::pdf_delete($path, $pages, $output)` | `{pages}` | remove 1-based pages; returns remaining count |
| `Office::pdf_extract($path, $pages, $output)` | `{ok, path, pages}` | keep only the selected pages into one PDF; `pages` array or range-spec string `"1-3,5,8-10"` (spec order) |
| `Office::pdf_remove_blank($path, $output)` | `{ok, path, removed, pages}` | remove pages whose extracted text is empty (text PDFs; never empties the doc) |
| `Office::pdf_reorder($path, $order, $output)` | `{pages}` | reorder/subset/repeat pages by a 1-based `order` list |
| `Office::pdf_attach($path, $file, $output, %opts)` | `{name, size, count}` | embed a file (EmbeddedFiles name tree); `name` overrides basename |
| `Office::pdf_attachments($path, %opts)` | `{attachments:[{name,size}], count}` | list embedded files; `extract_dir` writes them out |
| `Office::pdf_search($path, $query, %opts)` | `{count, matched_pages, pages:[{page,count,snippet}]}` | per-page full-text search; `regex`/`ignore_case` opts |
| `Office::pdf_burst($path, $dir, %opts)` | `{count, files}` | split into one PDF per page (`{prefix}-{n}.pdf`) |
| `Office::pdf_chunk($path, $size, $dir, %opts)` | `{count, files}` | split into fixed-size page chunks (last may be shorter) |
| `Office::pdf_split_ranges($path, $ranges, $dir, %opts)` | `{count, files}` | split into one file per `[start,end]` range |
| `Office::pdf_split_bookmarks($path, $dir, %opts)` | `{count, files}` | split at top-level bookmark boundaries (one file per chapter); `prefix` opt |
| `Office::pdf_to_text($path, %opts)` | `{pages, chars}` or `{count, files}` | extract text to one file (`output`) or per-page files (`dir`) |
| `Office::pdf_stats($path)` | `{pages, words, chars, chars_no_spaces, per_page}` | word/character statistics for a PDF (analogue of `doc_stats`) |
| `Office::pdf_assemble($inputs, $output)` | `{inputs, pages}` | build one PDF from a mix of image files (→ pages) and PDFs (merged) |
| `Office::pdf_stamp_image($path, $image, $output, %opts)` | `{stamped}` | overlay a logo/signature image on pages; `x`/`y`/`width`/`height`/`pages` |
| `Office::pdf_insert($path, $insert, $output, %opts)` | `{pages}` | splice another PDF's pages in after `position` |
| `Office::pdf_draw_rect($path, $rects, $output, %opts)` | `{pages, rects}` | draw filled/stroked rectangles on pages; `color`/`fill`/`pages` |
| `Office::pdf_add_text($path, $text, $output, %opts)` | `{pages}` | place text at `x`/`y` on pages (labels/stamps); `size`/`color`/`pages` |
| `Office::pdf_draw_line($path, $lines, $output, %opts)` | `{pages, lines}` | draw lines on pages (dividers/signature lines); `color`/`width`/`pages` |
| `Office::pdf_add_link($path, $url, %opts)` | `{ok, path, page}` | add a clickable URI link annotation; `page`/`rect` opts |
| `Office::pdf_links($path)` | `{links, count}` | extract every URI link annotation (`[{page, url, rect}]`) |
| `Office::pdf_remove_annotations($path, %opts)` | `{ok, path, removed}` | strip annotations (links/comments/highlights) to sanitize; `subtype`/`output` opts |
| `Office::pdf_highlight($path, $rect, %opts)` | `{ok, path, page}` | add a highlight annotation over a rectangle; `page`/`color`/`opacity` opts |
| `Office::pdf_annotations($path)` | `{annotations, count}` | list every annotation (`[{page, subtype, rect, contents?, uri?}]`) |
| `Office::pdf_form_fields($path)` | `{fields:[{name,type,value,options?}], count}` | list interactive AcroForm fields |
| `Office::pdf_fill_form($path, $values, %opts)` | `{filled}` | fill form fields; checkbox takes a bool; sets `/NeedAppearances` |
| `Office::pdf_outline($path)` | `{outline:[{title,page?,children?}], count}` | read the bookmark navigation tree |
| `Office::pdf_set_outline($path, $outline, %opts)` | `{count}` | set bookmarks from a nested `{title,page,children?}` spec |

#### PDF forms

```perl
# inspect a form, then fill it
my $f = Office::pdf_form_fields("application.pdf");   # name/type/value per field
Office::pdf_fill_form("application.pdf", {
    "applicant_name" => "Jane Doe",
    "date"           => "2026-06-13",
    "agree_terms"    => \1,                            # checkbox -> on-state
}, output => "application-filled.pdf");
```

Text/choice fields take a string; checkboxes/radios take a boolean (mapped to
the widget's on-state) or an explicit state name. Filling flips the AcroForm
`/NeedAppearances` flag so Acrobat and other conformant viewers regenerate the
visible field content.

#### PDF security

```perl
# password-protect with AES-128, allow printing only
Office::pdf_encrypt("report.pdf", "report-locked.pdf",
    owner_password => "admin",
    user_password  => "view",
    aes            => \1,
    permissions    => ["print"]);

# later: strip protection back off (owner or user password)
Office::pdf_decrypt("report-locked.pdf", "report-open.pdf", password => "admin");
```

The standard security handler is used: `aes => 1` selects AES-128 (V4),
otherwise RC4 at `key_length` bits (V2, default 128). `permissions` lists the
operations to *grant* (`print`, `modify`, `copy`, `annotate`, `fill`,
`accessibility`, `assemble`, `print_hq`); omit it to grant everything. A file
`/ID` is synthesized when absent so freshly built PDFs can be encrypted.
`pdf_compress` is the unrelated size pass — it prunes unreferenced objects and
deflates content into object streams, returning the byte delta.

#### PDF outline (bookmarks)

```perl
# add a navigation tree to a generated report
Office::pdf_set_outline("report.pdf", [
    { title => "Summary",  page => 1 },
    { title => "Details",  page => 2, bold => \1, children => [
        { title => "Q1", page => 2 },
        { title => "Q2", page => 4 },
    ]},
]);
my $toc = Office::pdf_outline("report.pdf");   # nested { title, page, children }
```

Page numbers are 1-based and resolved to page destinations; nodes nest via
`children`. Reading decodes UTF-16BE titles and follows `/Dest` and `/A` GoTo
actions back to a page number.

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
    sparklines => [{ at => [1,4], range => [1,0,1,3], type => "line",
                     markers => 1, high => 1, low => 1 }],   # in-cell mini chart
    group_rows => [[1,2]], group_columns => [[0,3]],          # outline grouping
    hide_rows => [5], hide_columns => [6], autofit => 1,
    rows => [[{ rich => [{ text => "Hot ", color => "#FF0000", bold => 1 },
                          { text => "cell" }] }]],          # multi-format cell
}],
    defined_names => [{ name => "Region", formula => "=S!\$A\$1" }],
    properties => { title => "Q Report", author => "jane", company => "MenkeTech" })

# docx styled table cells: bg, span (merge), width (dxa), valign
Office::doc_write("t.docx", [
    { kind => "table", rows => [
        [{ text => "Header", bold => 1, bg => "#D9E1F2", span => 2, valign => "center" }],
        [{ text => "A", width => 2400 }, { text => "B" }],
    ] },
])

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

### Document metadata (read + write, every format)

Core/app properties live in format-specific parts — OOXML `docProps/core.xml`
+ `docProps/app.xml`, ODF `meta.xml`, PDF Info dictionary. `meta_read` returns
a normalized key set from any of them; `meta_write` sets the keys you supply
and leaves the rest of the file byte-for-byte intact (lossless zip raw-copy for
containers, in-place Info-dict edit for PDF). Works on **xlsx/docx/pptx**,
**ods/odt/odp**, and **pdf**.

```perl
# read whatever the file carries
my $m = Office::meta_read("report.xlsx");
# $m->{title}, $m->{author}, $m->{company}, $m->{created}, ...

# set some keys, edit in place; existing properties are merged, not clobbered
Office::meta_write("report.xlsx", {
    title    => "Q2 Report",
    author   => "Jane Doe",
    subject  => "sales",
    keywords => "q2,sales,forecast",
    company  => "MenkeTechnologies",
});

# or write to a new file and convert ISO dates to the PDF date format
Office::meta_write("in.pdf", { title => "Spec", created => "2026-06-13T12:00:00Z" },
    output => "out.pdf");
```

Canonical keys: `title`, `author`, `subject`, `keywords`, `description`,
`category`, `last_modified_by`, `app`, `producer`, `company`, `created`,
`modified`. Each maps onto the right element/dict-key per format; keys a format
doesn't support are ignored.

### Embedded media extraction (document → image handles)

Office files keep their pictures as discrete parts — OOXML in `*/media/`, ODF in
`Pictures/`, PDF as image XObjects. `extract_images` pulls each one out and
**decodes it into a live image handle**, so extracted media flows straight into
the image surface (resize, convert, re-save). PDF JPEG (DCTDecode) streams are
lifted verbatim; raw device-RGB/Gray bitmaps are reconstructed.

```perl
# pull every picture out of a deck, write the originals to ./out, and make
# a 128px thumbnail of each
my $r = Office::extract_images("deck.pptx", dir => "out");
for my $im (@{ $r->{images} }) {
    Office::img_thumbnail($im->{handle}, 128);
    Office::img_save($im->{handle}, "out/thumb-$im->{name}");
}
# $r->{count} extracted, $r->{skipped} recognized-but-undecodable (e.g. JPEG2000)
```

Works on **xlsx/docx/pptx**, **ods/odt/odp**, and **pdf**. Each entry is
`{ name, handle, width, height, path? }` (`path` present when `dir` is given).

### Template filling (text search/replace)

`replace_text` fills `{{placeholder}}`-style templates across a document's
run-text parts. The hard part is OOXML: Word/Excel/PowerPoint routinely split a
single placeholder across several runs (`{{na` + `me}}`), so a per-node replace
misses it. This coalesces the runs **per paragraph** — join, replace, write the
result back into the first run — so a placeholder matches even when it was
fragmented. Paragraphs with no match are passed through untouched.

```perl
# fill an invoice template in place
Office::replace_text("invoice.docx", {
    "{{customer}}" => "Acme Corp",
    "{{total}}"    => "\$4,200.00",
    "{{date}}"     => "2026-06-13",
});

# or render to a new file
my $r = Office::replace_text("deck.pptx", { "{{quarter}}" => "Q2" },
    output => "deck-q2.pptx");
# $r->{replaced} == number of substitutions made
```

Covers **docx/pptx/xlsx** (document body, headers/footers, slides, notes,
shared + inline strings) and **ods/odt/odp** (content + styles). Replacements
are given as `{find => replacement}` or an ordered `replacements => [{find,
replace}]` list.

`Office::mail_merge($template, $dir, %opts)` runs that fill once per data
record — `data =>` a spreadsheet (read as records) or `records =>` a list of
hashes — emitting one document per row into `$dir`, named by `name_field` (or a
1-based index). Returns `{count, files}`.

For find/replace outside the binary office formats: `Office::sheet_replace`
edits spreadsheet **cells** (any format, incl. csv), and
`Office::text_replace($path, {find => repl}, %opts)` edits **plain-text** files
(md/html/txt/csv/json/rtf); both take `ignore_case`.

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

Chart types: `bar`/`column`, `stacked`, `line`, `area`, `stacked_area`,
`step`, `combo` (per-series `kind => "line"` overlays a line on bars),
`scatter` (`data` is `[[x,y],…]`; opt `trendline => 1` adds a least-squares
line), `bubble` (`[[x,y,size],…]`), `pie`, `donut`, `histogram` (opt `bins`),
`radar`, `sankey` (`nodes`/`links` instead of series), `waterfall` (deltas →
cumulative), `ohlc`/`candlestick` (`data` is `[[open,high,low,close],…]`),
`boxplot` (raw `data` → min/q1/median/q3/max), `funnel`, `gauge` (`value` +
`max`, no series), `heatmap` (`matrix => [[..],..]` or series-of-rows, no
series required), `treemap` (area-proportional), `polar`/rose, `bullet`
(per-series `{name, value, target, ranges}`), `pareto` (sorted bars +
cumulative-% line), `lollipop`/`dot`, `gantt` (per-series
`{name, start, end}` on a time axis), `sunburst` (multi-ring,
`rings => [[..],[..]]` innermost first), `range`/`range_column` (floating bars
from `data => [[lo,hi],…]`), `percent_stacked` (100%-stacked), `streamgraph`
(centered stacked area), `waffle` (10×10 share grid), `slope` (before/after),
`marimekko`/`mosaic` (variable-width stacked), `radial_bar` (concentric arcs),
`calendar` (GitHub-style heatmap from `values`, no series), `parallel`
(parallel coordinates — each series a row across dimension axes), `hexbin`
(scatter density in hexagonal cells), `density` (ggplot2 `geom_density` — one
Gaussian-KDE curve per series of raw `data`, Silverman bandwidth, opt `points`
grid resolution), `violin` (ggplot2 `geom_violin` — width-normalized mirrored
KDE per series with a median tick, shared value axis), `ecdf` (ggplot2
`stat_ecdf` — right-continuous empirical-CDF step curve per series, y from 0→1),
`qq`/`qqplot` (ggplot2 `stat_qq` + `geom_qq_line` — sample vs theoretical
standard-normal quantiles with a quartile reference line), `ribbon` (ggplot2
`geom_ribbon` — continuous filled band between `[[lo,hi],…]` per x, distinct
from `range`'s discrete floating bars), `jitter`/`strip` (ggplot2 `geom_jitter`
— per-series category strip of raw `data`, `jitter` spreads x with a seeded PRNG
(`seed`/`jitter_width`), `strip` keeps points centered), `rug` (ggplot2
`geom_rug` — marginal tick per raw value along the value axis, one lane per
series), `beeswarm` (collision-avoiding point swarm whose silhouette encodes the
distribution — ggbeeswarm `geom_beeswarm`; opt `radius`), `contour`/`density2d`
(ggplot2 `geom_density_2d` — marching-squares iso-density contour lines from a
2-D Gaussian KDE of scatter `[[x,y],…]`; opts `grid`/`levels`),
`ridgeline`/`ridge`/`joyplot` (ggridges `geom_density_ridges` — per-series KDE
ridges stacked on their own baselines; opts `points`/`overlap`), `smooth`/`loess`
(ggplot2 `geom_smooth` — locally weighted (LOESS) regression curve over scatter
`[[x,y],…]`; opts `span`/`points`; distinct from scatter's linear `trendline`),
`bin2d` (ggplot2 `geom_bin2d` — rectangular 2-D count heatmap of pooled scatter
points; opts `bins`/`xbins`/`ybins`; the square-cell counterpart to `hexbin`),
`pairs`/`splom`/`scattermatrix` (base R `pairs()` — m×m scatterplot matrix where
each series is one variable column `data => [v1,v2,…]`, names on the diagonal),
`dendrogram`/`hclust`/`cluster` (base R `hclust` — agglomerative average-linkage
Euclidean clustering tree of observations, each series a feature vector; y axis
is merge height).
opts: `title`, `width` (800), `height` (600), `categories`, per-series
`color`, `legend => 0` to suppress, `labels => 1` for data labels, `x_label`,
`y_label`, `markers => 1` (line family), `reference_lines => [{y, color}]`,
`smooth => 1` (Catmull-Rom spline lines), `palette => ["#…"]` (custom color
cycle), `background => "#…"` (canvas), `log_y => 1` (logarithmic Y axis when
the range is all-positive), per-series `errors => [e,…]` (error-bar whiskers),
`annotations => [{x, y, text, color}]` (point callouts). Every type renders
identically in raster **and** SVG.

**Raster and vector output, any format.** Three entry points:
- `chart_render(type, series, %opts)` → raster image handle (then `img_save`
  to png/jpeg/tif/bmp/webp/gif, or process further).
- `chart_svg(type, series, %opts)` → vector **SVG** markup (or write to
  `path =>`).
- `chart_save(type, path, %opts)` → write straight to a file, format by
  extension: `.svg` (vector), `.pdf` (chart embedded in a PDF), or any raster
  extension.
- `chart_from_sheet(path, output, %opts)` → read a spreadsheet's columns and
  render a chart in one call; `categories` column for x labels, `series`
  columns (auto-detected numeric by default), format by `output` extension.
- `chart_grid(charts, %opts)` → render many specs and tile them into one
  **dashboard** image (cols/cell_width/cell_height/gap/background); `path =>`
  saves the grid straight to any raster extension or `.pdf`. The "tons of
  graphs in one artifact" path.

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
| `Office::img_scale($h, $factor, %opts)` | info | resize by a scale factor (preserve aspect); `filter` opt |
| `Office::img_fit($h, $w, $ht, %opts)` | info | letterbox-fit into an exact canvas (preserve aspect, center on background); `color`/`filter` opts |
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
| `img_concat($handles, %opts)` | edge-to-edge concat (flush, native sizes) → new handle; `axis` h/v, `gap`/`bg` |
| `img_canvas($handle, $width, $height, %opts)` | resize the canvas to an exact size, anchoring the unscaled image; `anchor`/`color` opts |
| `img_gradient($h, %opts)` | fill `linear`/`radial` between `from`/`to`; `angle` |
| `img_draw_ellipse($h, $x, $y, $rx, $ry, $color, %opts)` | `fill` opt |
| `img_draw_polygon($h, $points, $color)` | `points` = `[[x,y],…]` |
| `img_draw_text_multiline($h, $x, $y, $text, $color, %opts)` | splits on `\n`; `size`/`line_height`/`font` |
| `img_warp($h, $matrix)` | 3×3 projective (affine/perspective); 9 numbers |
| `img_to_base64($h, %opts)` / `img_from_base64($b64)` | encode/decode (`format` opt); embed images as strings |
| `img_data_uri($path)` | encode an image file as a `data:` URI for inline HTML/CSS → `{data_uri, mime, bytes}` |
| `img_resize_file($input, $output, %opts)` | open+resize+save in one call; `max` (fit) or `width`+`height` (exact); transcodes by ext |
| `contact_sheet($images, $output, %opts)` | thumbnail-grid image from a list of files; `thumb`/`cols`/`gap`/`bg` |

#### Shapes, fills, masks, color analysis

| Function | Notes |
|---|---|
| `img_draw_rounded_rect($h, $x, $y, $w, $ht, $color, %opts)` | `radius`/`fill`/`stroke` |
| `img_draw_polyline($h, $points, $color)` | open polyline through `[[x,y],…]` |
| `img_draw_arc($h, $x, $y, $r, $start, $end, $color, %opts)` | degrees; `fill` → wedge |
| `img_flood_fill($h, $x, $y, $color, %opts)` | bucket fill; `tolerance` |
| `img_replace_color($h, $from, $to, %opts)` | global color replace; `tolerance` |
| `img_swap_channels($h, $order)` | permute r/g/b/a, e.g. `"bgr"` |
| `img_dominant_colors($h, %opts)` | top-`count` palette → `[{r,g,b,hex,count}]` |
| `img_compare($h, $other, %opts)` | `{mse,rmse,max_diff,identical}`; `diff` → `diff_handle` |
| `img_text_size($text, %opts)` | measure → `{width,height}` |
| `img_caption($input, $output, $text, %opts)` | add a caption bar with centered text above/below an image; `position`/`height`/`size`/`color`/`background` opts |
| `img_crop_circle($h)` / `img_round_corners($h, %opts)` | circular / rounded mask |
| `img_drop_shadow($h, %opts)` | soft shadow; `dx`/`dy`/`blur`/`color`/`opacity` |

#### Color science & distortions

| Function | Notes |
|---|---|
| `img_levels($h, %opts)` | per-channel `in_black`/`in_white`/`gamma`/`out_black`/`out_white` |
| `img_curves($h, $points)` | tone curve from `[[x,y],…]` control points |
| `img_hsl($h, %opts)` | `hue` shift / `saturation` / `lightness` multipliers |
| `img_temperature($h, $amount)` | warm/cool white balance (−100..100) |
| `img_channel_mixer($h, $matrix)` | 3×3 RGB mix matrix |
| `img_swirl($h, %opts)` | swirl; `strength`/`radius` |
| `img_wave($h, %opts)` | sinusoidal ripple; `amplitude`/`wavelength`/`axis` |
| `img_fisheye($h, %opts)` | barrel distortion; `strength` |
| `img_kaleidoscope($h, %opts)` | mirror wedges; `segments` |
| `img_spritesheet($h, %opts)` | split into `cols`×`rows` handles |
| `img_seam_carve($h, $width)` | content-aware width reduction |
| `img_dither($h, %opts)` | Floyd–Steinberg to `levels` steps/channel |
| `img_quantize($h, %opts)` | median-cut to `colors` palette + remap → `{colors}` |
| `img_favicon($h, $path, %opts)` | multi-resolution `.ico` (`sizes`, default 16/32/48) |

### Barcodes & QR codes (data → image handle → any format)

Both produce an image handle, so the result composes with the entire image
surface — save to any raster format, `img_paste` onto a label canvas, or embed
in a PDF (`pdf_build`) or docx. No separate output path.

| Function | Notes |
|---|---|
| `barcode_qr(%opts)` | QR code; `data`, `ec` (L/M/Q/H, default M), `scale` px/module (6), `quiet` modules (4), `fg`/`bg` → `{handle, width, height, modules}` |
| `barcode_1d(%opts)` | 1D barcode; `data`, `symbology` (default `code128`), `scale` px/bar (2), `height` px (80), `quiet` px, `fg`/`bg`, `set` (Code128 A/B/C) → `{handle, width, height, symbology, bars}` |
| `barcode_save($data, $output, %opts)` | generate a barcode straight to an image file; `kind` qr/1d, plus the matching generator options → `{ok, path, kind, width, height}` |
| `barcode_sheet($path, $column, $dir, %opts)` | batch-generate one barcode/QR per column value into a directory; `kind`/`symbology`/`ext`/`prefix` + style opts → `{count, files}` |

Supported `symbology` values: `code128`, `code39`, `code93`, `code11`,
`codabar`, `ean13`, `ean8`, `upca`, `itf` (interleaved 2-of-5), `std2of5`.

```perl
# QR for a URL, saved as PNG
my $qr = Office::barcode_qr(data => "https://example.com", ec => "H", scale => 8);
Office::img_save($qr->{handle}, path => "qr.png");

# Code128 label embedded straight into a PDF
my $bc = Office::barcode_1d(symbology => "code128", data => "SKU-00421", height => 90);
Office::pdf_build(path => "label.pdf", pages => [{ elements => [
  { image => $bc->{handle}, x => 40, y => 60 },
] }]);
```

Generated natively with `qrcode` (matrix) + `barcoders` (1D) — no `zint`, no
ImageMagick, no subprocess.

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
| pdf merge / split / rotate / info / encrypt / decrypt / compress | `lopdf` (pure-Rust core, `default-features = false`) |
| image read + write (all formats) | `image` |
| image drawing (shapes) | `imageproc` |
| image text drawing | `ab_glyph` + vendored `assets/DejaVuSans.ttf` |
| QR-code generation | `qrcode` (matrix only, no second image-crate pin) |
| 1D barcode generation | `barcoders` (`default-features = false`, `std`) |

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
  src/barcode.rs         # QR + 1D barcode generation (-> image handle)
  src/meta_ops.rs        # document metadata read/write (OOXML/ODF/PDF)
  src/extract.rs         # embedded media extraction (-> image handles)
  src/textops.rs         # template text search/replace (run-coalescing)
  src/doc_struct.rs      # structured document reads (tables, blocks, hyperlinks, stats) + doc_merge/convert
  src/pdf_build.rs       # multi-element paginated PDF document builder
  src/pdf_ops.rs         # PDF merge/split/rotate/info/encrypt/decrypt/compress/delete/reorder (lopdf)
  src/pdf_attach.rs      # PDF file attachments: embed + list/extract (lopdf)
  src/pdf_form.rs        # PDF AcroForm field list + fill (lopdf)
  src/pdf_outline.rs     # PDF outline/bookmarks read + write (lopdf)
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

- PowerPoint speaker-notes *write* + images on slides (notesMaster/notesSlide
  OOXML); notes *read* is done (`slides_read` → `notes`).
- Spreadsheet formula *evaluation* on read (formula **strings** already read
  via `formulas => 1`; values are computed by the writing app).
- Per-run styling on ODF (ods/odt) write — blocked on `lo_odf` exposing it.
- xls (legacy binary) write.

## [0xFF] License

MIT. See [LICENSE](LICENSE).
