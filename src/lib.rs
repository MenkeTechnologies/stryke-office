//! stryke-office — Office/OpenDocument/PDF import+export cdylib for stryke.
//!
//! Loaded in-process via dlopen on first `use Office`. Each
//! `#[no_mangle] extern "C" fn office__*` is a JSON-string-in /
//! JSON-string-out wrapper, the same ABI as the other stryke cdylibs.
//!
//! Everything is native — there is NO subprocess to LibreOffice / soffice /
//! pandoc. Formats are handled by vendored Rust crates:
//!   * spreadsheets — read xlsx/ods/xls + csv (`calamine`), write xlsx
//!     (`rust_xlsxwriter`) / ods (`lo_odf`)
//!   * word processing — read docx/odt (`zip` + `quick-xml`), write docx
//!     (`docx-rs`) / odt (`lo_odf`)
//!   * presentations — read pptx/odp (`zip` + `quick-xml`), write odp
//!     (`lo_odf`) / pptx (`zip` + hand-built OOXML)
//!   * pdf — read + write text (`lo_core`, self-contained, no font files)
//!
//! Output format is chosen from the path's extension (override with `format`).

// The pixel and chart-rendering code is written with index-addressed loops over
// parallel arrays — RGBA channels, the category axis, running stack
// accumulators — where indexing is the clearest form; and the renderers take
// many positional layout bounds (l/t/r/b + data + style) plus closures over
// them. These are deliberate, readable choices for numeric/plotting code, not
// defects, so the corresponding style lints are allowed crate-wide.
#![allow(clippy::needless_range_loop)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]

use std::ffi::{CStr, CString};
use std::io::{Cursor, Read, Write};
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;
use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

// ── format detection ─────────────────────────────────────────────────────────

fn ext_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn target_ext(opts: &Value, path: &str) -> String {
    opts.get("format")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| ext_of(path))
}

// ── arg helpers ──────────────────────────────────────────────────────────────

fn req_str<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing {key}"))
}

/// A JSON cell value -> display string (used by writers that take text).
fn cell_to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Lenient boolean reader, drop-in for `Value::as_bool` in `.and_then(...)`.
/// stryke has no distinct boolean type (Perl heritage), so callers pass `1`/`0`
/// which serialize as JSON integers, not `true`/`false`; treat any non-zero
/// number and "true"/"1"/"yes"/"on" as true so flags work from idiomatic stryke.
fn flag_of(v: &Value) -> Option<bool> {
    Some(match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on"),
        Value::Null => return None,
        _ => false,
    })
}

/// Read a boolean option leniently from a container by key (see `flag_of`).
fn opt_flag(opts: &Value, key: &str) -> Option<bool> {
    opts.get(key).and_then(flag_of)
}

// ── zip + xml helpers (OOXML / ODF readers) ──────────────────────────────────

fn read_zip_entry(bytes: &[u8], name: &str) -> Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
    let mut f = zip
        .by_name(name)
        .map_err(|_| anyhow!("entry not found: {name}"))?;
    let mut out = Vec::new();
    f.read_to_end(&mut out)?;
    Ok(out)
}

fn zip_entry_names(bytes: &[u8]) -> Result<Vec<String>> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
    Ok((0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .collect())
}

/// In an OOXML `.rels` part, return the `Target` of the first `Relationship`
/// whose `Type` ends with `type_suffix` (e.g. `"notesSlide"`, `"hyperlink"`).
fn rels_relationship_target(rels_xml: &[u8], type_suffix: &str) -> Option<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(rels_xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"Relationship" => {
                if attr(&e, b"Type")
                    .map(|t| t.ends_with(type_suffix))
                    .unwrap_or(false)
                {
                    return attr(&e, b"Target");
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// Build an `Id` → `Target` map of every `Relationship` in a `.rels` part.
/// `Target` values are XML-unescaped (so `&amp;` in a URL comes back as `&`).
fn rels_id_target_map(rels_xml: &[u8]) -> std::collections::HashMap<String, String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(rels_xml);
    let mut buf = Vec::new();
    let mut map = std::collections::HashMap::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"Relationship" => {
                let mut id = None;
                let mut target = None;
                for a in e.attributes().flatten() {
                    match a.key.as_ref() {
                        b"Id" => id = Some(String::from_utf8_lossy(&a.value).into_owned()),
                        b"Target" => {
                            target = a
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .ok()
                                .map(|c| c.into_owned())
                        }
                        _ => {}
                    }
                }
                if let (Some(i), Some(t)) = (id, target) {
                    map.insert(i, t);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    map
}

/// Resolve a relationship `Target` (which may be `../`-relative or root-absolute
/// `/...`) against the directory of the part that referenced it, into a
/// zip-entry path (e.g. base `ppt/slides`, target `../notesSlides/n1.xml` →
/// `ppt/notesSlides/n1.xml`).
fn resolve_zip_path(base_dir: &str, target: &str) -> String {
    let mut parts: Vec<&str> = if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    } else {
        base_dir.split('/').filter(|s| !s.is_empty()).collect()
    };
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Extract the text of each paragraph-level element. `para_tags` are the
/// fully-qualified element names that delimit a paragraph (e.g. `w:p`,
/// `text:p`, `a:p`); all text nodes nested inside one are concatenated.
/// Resolve a quick-xml entity reference (`Event::GeneralRef`) to its character:
/// numeric char refs (`&#N;`/`&#xN;`) and the five predefined XML entities.
/// quick-xml emits these as standalone events (not inside `Text`), so every text
/// extractor must handle them or `&`/`<`/`>`/`"`/`'` in content silently vanish.
fn xml_ref_char(e: &quick_xml::events::BytesRef) -> Option<char> {
    if let Ok(Some(c)) = e.resolve_char_ref() {
        return Some(c);
    }
    let name: &[u8] = e;
    match name {
        b"amp" => Some('&'),
        b"lt" => Some('<'),
        b"gt" => Some('>'),
        b"quot" => Some('"'),
        b"apos" => Some('\''),
        _ => None,
    }
}

fn extract_paragraphs(xml: &[u8], para_tags: &[&str]) -> Vec<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let matches = |n: &[u8]| para_tags.iter().any(|t| t.as_bytes() == n);
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if matches(e.name().as_ref()) {
                    depth += 1;
                }
            }
            Ok(Event::End(e)) => {
                if matches(e.name().as_ref()) {
                    depth -= 1;
                    if depth == 0 {
                        out.push(std::mem::take(&mut cur));
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if depth > 0 {
                    if let Ok(t) = e.xml10_content() {
                        cur.push_str(&t);
                    }
                }
            }
            Ok(Event::GeneralRef(e)) => {
                if depth > 0 {
                    if let Some(c) = xml_ref_char(&e) {
                        cur.push(c);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── spreadsheets ─────────────────────────────────────────────────────────────

/// Native ODS reader (zip + quick-xml). calamine's ODS parser is strict
/// about inter-element whitespace and rejects pretty-printed content.xml
/// (which `lo_odf` emits), so we parse the OpenDocument table model directly:
/// `table:table` -> sheet, `table:table-row` -> row, `table:table-cell` ->
/// cell. `table:number-columns-repeated` is honoured; a float `office:value`
/// comes back as a JSON number, everything else as the cell's text.
fn read_ods(path: &str) -> Result<Value> {
    use quick_xml::events::Event;
    let bytes = std::fs::read(path)?;
    let xml = read_zip_entry(&bytes, "content.xml")?;
    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    let mut buf = Vec::new();
    let mut sheets: Vec<Value> = Vec::new();
    let mut sheet_name = String::new();
    let mut rows: Vec<Value> = Vec::new();
    let mut row: Vec<Value> = Vec::new();
    let mut in_cell = false;
    let mut cell_text = String::new();
    let mut cell_repeat = 1usize;
    let mut cell_float: Option<f64> = None;

    // Finalize the current cell into `row`, honouring the repeat count.
    let finish_cell = |row: &mut Vec<Value>, text: &str, float: Option<f64>, repeat: usize| {
        let val = match float {
            Some(f) => json!(f),
            None if text.is_empty() => Value::Null,
            None => Value::String(text.to_string()),
        };
        for _ in 0..repeat.min(4096) {
            row.push(val.clone());
        }
    };

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"table:table" => {
                    sheet_name = attr(&e, b"table:name").unwrap_or_default();
                    rows.clear();
                }
                b"table:table-row" => row = Vec::new(),
                b"table:table-cell" | b"table:covered-table-cell" => {
                    in_cell = true;
                    cell_text.clear();
                    cell_repeat = attr(&e, b"table:number-columns-repeated")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1);
                    cell_float = cell_float_of(&e);
                }
                _ => {}
            },
            // Self-closing cell (e.g. empty `<table:table-cell/>`): no End event.
            Ok(Event::Empty(e)) => {
                if matches!(
                    e.name().as_ref(),
                    b"table:table-cell" | b"table:covered-table-cell"
                ) {
                    let repeat = attr(&e, b"table:number-columns-repeated")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1);
                    finish_cell(&mut row, "", cell_float_of(&e), repeat);
                }
            }
            Ok(Event::Text(e)) => {
                if in_cell {
                    if let Ok(t) = e.xml10_content() {
                        cell_text.push_str(&t);
                    }
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"table:table-cell" | b"table:covered-table-cell" => {
                    in_cell = false;
                    finish_cell(&mut row, &cell_text, cell_float, cell_repeat);
                }
                b"table:table-row" => {
                    while row.last() == Some(&Value::Null) {
                        row.pop();
                    }
                    rows.push(Value::Array(std::mem::take(&mut row)));
                }
                b"table:table" => {
                    while rows
                        .last()
                        .and_then(|r| r.as_array())
                        .is_some_and(|r| r.is_empty())
                    {
                        rows.pop();
                    }
                    sheets.push(json!({"name": sheet_name, "rows": rows.clone()}));
                    rows.clear();
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(json!({ "sheets": sheets }))
}

/// A numeric ODF cell value, if the cell declares a float-like value type.
fn cell_float_of(e: &quick_xml::events::BytesStart) -> Option<f64> {
    match attr(e, b"office:value-type").as_deref() {
        Some("float") | Some("percentage") | Some("currency") => {
            attr(e, b"office:value").and_then(|s| s.parse().ok())
        }
        _ => None,
    }
}

fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if a.key.as_ref() == key {
            Some(String::from_utf8_lossy(&a.value).into_owned())
        } else {
            None
        }
    })
}

fn op_sheet_read(opts: Value) -> Result<Value> {
    use calamine::{open_workbook_auto, Data, Reader};
    let path = req_str(&opts, "path")?;
    let delim = opts
        .get("delimiter")
        .and_then(Value::as_str)
        .and_then(|s| s.chars().next());
    match ext_of(path).as_str() {
        "ods" => return read_ods(path),
        "csv" => return read_csv(path, delim.unwrap_or(',')),
        "tsv" => return read_csv(path, delim.unwrap_or('\t')),
        _ => {}
    }
    let want_formulas = opts.get("formulas").and_then(flag_of).unwrap_or(false);
    let mut wb = open_workbook_auto(path)?;
    let names = wb.sheet_names().to_owned();
    let mut sheets = Vec::new();
    for name in names {
        let range = wb
            .worksheet_range(&name)
            .map_err(|e| anyhow!("sheet {name}: {e}"))?;
        let rows: Vec<Value> = range
            .rows()
            .map(|row| {
                Value::Array(
                    row.iter()
                        .map(|c| match c {
                            Data::Empty => Value::Null,
                            Data::String(s) => Value::String(s.clone()),
                            Data::Float(f) => json!(f),
                            Data::Int(i) => json!(i),
                            Data::Bool(b) => Value::Bool(*b),
                            Data::DateTime(d) => json!(d.as_f64()),
                            Data::DateTimeIso(s) => Value::String(s.clone()),
                            Data::DurationIso(s) => Value::String(s.clone()),
                            Data::Error(e) => Value::String(format!("#ERR {e:?}")),
                        })
                        .collect(),
                )
            })
            .collect();
        let mut sheet = json!({"name": name, "rows": rows});
        // When requested, also return formula strings aligned to absolute cell
        // coordinates (calamine's formula range starts at its own used area, so
        // we offset by its top-left to line up with `rows`). Empty cell => "".
        if want_formulas {
            if let Ok(fr) = wb.worksheet_formula(&name) {
                let (r0, c0) = fr.start().unwrap_or((0, 0));
                let mut grid: Vec<Vec<String>> = Vec::new();
                for (ri, row) in fr.rows().enumerate() {
                    let abs_r = r0 as usize + ri;
                    while grid.len() <= abs_r {
                        grid.push(Vec::new());
                    }
                    for (ci, f) in row.iter().enumerate() {
                        let abs_c = c0 as usize + ci;
                        let line = &mut grid[abs_r];
                        while line.len() <= abs_c {
                            line.push(String::new());
                        }
                        line[abs_c] = f.clone();
                    }
                }
                sheet["formulas"] = json!(grid);
            }
        }
        sheets.push(sheet);
    }
    Ok(json!({ "sheets": sheets }))
}

fn json_sheets(opts: &Value) -> Result<Vec<(String, Vec<Vec<Value>>)>> {
    let arr = opts
        .get("sheets")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing sheets (expected array)"))?;
    let mut out = Vec::new();
    for (i, s) in arr.iter().enumerate() {
        let name = s
            .get("name")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("Sheet{}", i + 1));
        let rows = s
            .get("rows")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("sheet {name}: missing rows"))?
            .iter()
            .map(|r| r.as_array().cloned().unwrap_or_default())
            .collect();
        out.push((name, rows));
    }
    Ok(out)
}

fn xlsx_color(s: &str) -> rust_xlsxwriter::Color {
    let n = u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0);
    rust_xlsxwriter::Color::RGB(n)
}

/// Build a cell Format from a rich-cell object's styling keys, or None if it
/// carries no formatting. Keys: bold, italic, underline, font, size, color,
/// bg, align ("left"/"center"/"right"), num_format, border.
fn xlsx_format(o: &serde_json::Map<String, Value>) -> Option<rust_xlsxwriter::Format> {
    use rust_xlsxwriter::{Format, FormatAlign, FormatBorder};
    let mut f = Format::new();
    let mut used = false;
    if o.get("bold").and_then(flag_of) == Some(true) {
        f = f.set_bold();
        used = true;
    }
    if o.get("italic").and_then(flag_of) == Some(true) {
        f = f.set_italic();
        used = true;
    }
    if o.get("underline").and_then(flag_of) == Some(true) {
        f = f.set_underline(rust_xlsxwriter::FormatUnderline::Single);
        used = true;
    }
    if let Some(name) = o.get("font").and_then(Value::as_str) {
        f = f.set_font_name(name);
        used = true;
    }
    if let Some(sz) = o.get("size").and_then(Value::as_f64) {
        f = f.set_font_size(sz);
        used = true;
    }
    if let Some(c) = o.get("color").and_then(Value::as_str) {
        f = f.set_font_color(xlsx_color(c));
        used = true;
    }
    if let Some(c) = o.get("bg").and_then(Value::as_str) {
        f = f.set_background_color(xlsx_color(c));
        used = true;
    }
    if let Some(a) = o.get("align").and_then(Value::as_str) {
        let align = match a {
            "center" => FormatAlign::Center,
            "right" => FormatAlign::Right,
            "left" => FormatAlign::Left,
            _ => FormatAlign::General,
        };
        f = f.set_align(align);
        used = true;
    }
    if let Some(nf) = o.get("num_format").and_then(Value::as_str) {
        f = f.set_num_format(nf);
        used = true;
    }
    if o.get("border").and_then(flag_of) == Some(true) {
        f = f.set_border(FormatBorder::Thin);
        used = true;
    }
    used.then_some(f)
}

fn quad_arr(v: &Value) -> Option<[u32; 4]> {
    let a = v.as_array()?;
    if a.len() < 4 {
        return None;
    }
    Some([
        a[0].as_u64()? as u32,
        a[1].as_u64()? as u32,
        a[2].as_u64()? as u32,
        a[3].as_u64()? as u32,
    ])
}

fn quad(v: &Value, key: &str) -> Option<[u32; 4]> {
    quad_arr(v.get(key)?)
}

/// Write xlsx from raw sheet objects, honouring sheet-level structure:
/// `{name, rows, merges:[[r1,c1,r2,c2]], cols:[{col,width}],
///   row_heights:[{row,height}], freeze:[row,col], autofilter:[r1,c1,r2,c2],
///   table:[r1,c1,r2,c2]}`. Cells may be scalars, rich objects, or
/// `{link, v}` hyperlinks.
fn write_xlsx(path: &str, sheets: &[Value], opts: &Value) -> Result<()> {
    use rust_xlsxwriter::{Format, Table, Workbook};
    let mut wb = Workbook::new();
    for (i, s) in sheets.iter().enumerate() {
        let name = s
            .get("name")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("Sheet{}", i + 1));
        let empty = Vec::new();
        let rows = s.get("rows").and_then(Value::as_array).unwrap_or(&empty);
        let ws = wb.add_worksheet();
        ws.set_name(&name)?;

        // Merged ranges: skip per-cell writes inside them; merge_range writes
        // the top-left value.
        let merges: Vec<[u32; 4]> = s
            .get("merges")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| {
                        let q = m.as_array()?;
                        Some([
                            q.first()?.as_u64()? as u32,
                            q.get(1)?.as_u64()? as u32,
                            q.get(2)?.as_u64()? as u32,
                            q.get(3)?.as_u64()? as u32,
                        ])
                    })
                    .collect()
            })
            .unwrap_or_default();
        let in_merge = |r: u32, c: u32| {
            merges
                .iter()
                .any(|m| r >= m[0] && r <= m[2] && c >= m[1] && c <= m[3])
        };

        for (r, row) in rows.iter().enumerate() {
            if let Some(cells) = row.as_array() {
                for (c, cell) in cells.iter().enumerate() {
                    let (r, c) = (r as u32, c as u16);
                    if in_merge(r, c as u32) {
                        continue;
                    }
                    write_xlsx_cell(ws, r, c, cell)?;
                }
            }
        }

        for m in &merges {
            let text = rows
                .get(m[0] as usize)
                .and_then(|r| r.as_array())
                .and_then(|cells| cells.get(m[1] as usize))
                .map(|cell| match cell {
                    Value::Object(o) => cell_to_string(
                        o.get("v")
                            .or_else(|| o.get("value"))
                            .unwrap_or(&Value::Null),
                    ),
                    other => cell_to_string(other),
                })
                .unwrap_or_default();
            ws.merge_range(m[0], m[1] as u16, m[2], m[3] as u16, &text, &Format::new())?;
        }

        if let Some(cols) = s.get("cols").and_then(Value::as_array) {
            for c in cols {
                if let (Some(col), Some(w)) = (
                    c.get("col").and_then(Value::as_u64),
                    c.get("width").and_then(Value::as_f64),
                ) {
                    ws.set_column_width(col as u16, w)?;
                }
            }
        }
        if let Some(rhs) = s.get("row_heights").and_then(Value::as_array) {
            for rh in rhs {
                if let (Some(row), Some(h)) = (
                    rh.get("row").and_then(Value::as_u64),
                    rh.get("height").and_then(Value::as_f64),
                ) {
                    ws.set_row_height(row as u32, h)?;
                }
            }
        }
        if let Some(f) = s.get("freeze").and_then(Value::as_array) {
            if let (Some(row), Some(col)) = (
                f.first().and_then(Value::as_u64),
                f.get(1).and_then(Value::as_u64),
            ) {
                ws.set_freeze_panes(row as u32, col as u16)?;
            }
        }
        if let Some(a) = quad(s, "autofilter") {
            ws.autofilter(a[0], a[1] as u16, a[2], a[3] as u16)?;
        }
        if let Some(t) = quad(s, "table") {
            ws.add_table(t[0], t[1] as u16, t[2], t[3] as u16, &Table::new())?;
        }
        write_xlsx_charts(ws, &name, s)?;
        write_xlsx_cond_val(ws, s)?;
        write_xlsx_setup(ws, &name, s)?;
    }
    // Workbook document properties: `properties:{title, author, subject,
    // company, manager, keywords, comments, category, status}`.
    if let Some(p) = opts.get("properties").and_then(Value::as_object) {
        use rust_xlsxwriter::DocProperties;
        let mut props = DocProperties::new();
        let g = |k: &str| p.get(k).and_then(Value::as_str);
        if let Some(v) = g("title") {
            props = props.set_title(v);
        }
        if let Some(v) = g("author") {
            props = props.set_author(v);
        }
        if let Some(v) = g("subject") {
            props = props.set_subject(v);
        }
        if let Some(v) = g("company") {
            props = props.set_company(v);
        }
        if let Some(v) = g("manager") {
            props = props.set_manager(v);
        }
        if let Some(v) = g("keywords") {
            props = props.set_keywords(v);
        }
        if let Some(v) = g("comments") {
            props = props.set_comment(v);
        }
        if let Some(v) = g("category") {
            props = props.set_category(v);
        }
        if let Some(v) = g("status") {
            props = props.set_status(v);
        }
        wb.set_properties(&props);
    }
    // Workbook-level defined names: `defined_names:[{name, formula}]`.
    if let Some(dn) = opts.get("defined_names").and_then(Value::as_array) {
        for d in dn {
            if let (Some(name), Some(formula)) = (
                d.get("name").and_then(Value::as_str),
                d.get("formula").and_then(Value::as_str),
            ) {
                wb.define_name(name, formula)?;
            }
        }
    }
    wb.save(path)?;
    Ok(())
}

/// Per-sheet page setup, protection, and embedded content:
///   `protect:bool, tab_color:"#rgb", zoom:u16, landscape:bool, paper:u8,
///    print_gridlines:bool, print_area:[r1,c1,r2,c2], repeat_rows:[first,last],
///    header:str, footer:str, margins:[l,r,t,b,h,f],
///    notes:[{row,col,text,author?}], images:[{row,col,path}]`.
fn write_xlsx_setup(ws: &mut rust_xlsxwriter::Worksheet, sheet: &str, s: &Value) -> Result<()> {
    use rust_xlsxwriter::{Image, Note};
    if s.get("protect").and_then(flag_of) == Some(true) {
        ws.protect();
    }
    if let Some(c) = s.get("tab_color").and_then(Value::as_str) {
        ws.set_tab_color(xlsx_color(c));
    }
    if let Some(z) = s.get("zoom").and_then(Value::as_u64) {
        ws.set_zoom(z as u16);
    }
    if s.get("landscape").and_then(flag_of) == Some(true) {
        ws.set_landscape();
    }
    if let Some(p) = s.get("paper").and_then(Value::as_u64) {
        ws.set_paper_size(p as u8);
    }
    if s.get("print_gridlines").and_then(flag_of) == Some(true) {
        ws.set_print_gridlines(true);
    }
    if let Some(h) = s.get("header").and_then(Value::as_str) {
        ws.set_header(h);
    }
    if let Some(f) = s.get("footer").and_then(Value::as_str) {
        ws.set_footer(f);
    }
    if let Some(a) = quad(s, "print_area") {
        ws.set_print_area(a[0], a[1] as u16, a[2], a[3] as u16)?;
    }
    if let Some(rr) = s.get("repeat_rows").and_then(Value::as_array) {
        if let (Some(f), Some(l)) = (
            rr.first().and_then(Value::as_u64),
            rr.get(1).and_then(Value::as_u64),
        ) {
            ws.set_repeat_rows(f as u32, l as u32)?;
        }
    }
    if let Some(m) = s.get("margins").and_then(Value::as_array) {
        let g = |i: usize, d: f64| m.get(i).and_then(Value::as_f64).unwrap_or(d);
        ws.set_margins(
            g(0, 0.7),
            g(1, 0.7),
            g(2, 0.75),
            g(3, 0.75),
            g(4, 0.3),
            g(5, 0.3),
        );
    }
    if let Some(notes) = s.get("notes").and_then(Value::as_array) {
        for n in notes {
            if let (Some(row), Some(col), Some(text)) = (
                n.get("row").and_then(Value::as_u64),
                n.get("col").and_then(Value::as_u64),
                n.get("text").and_then(Value::as_str),
            ) {
                let mut note = Note::new(text);
                if let Some(author) = n.get("author").and_then(Value::as_str) {
                    note = note.set_author(author);
                }
                ws.insert_note(row as u32, col as u16, &note)?;
            }
        }
    }
    if let Some(imgs) = s.get("images").and_then(Value::as_array) {
        for im in imgs {
            if let (Some(row), Some(col), Some(path)) = (
                im.get("row").and_then(Value::as_u64),
                im.get("col").and_then(Value::as_u64),
                im.get("path").and_then(Value::as_str),
            ) {
                let image = Image::new(path).map_err(|e| anyhow!("image {path}: {e}"))?;
                ws.insert_image(row as u32, col as u16, &image)?;
            }
        }
    }
    write_xlsx_sparklines(ws, sheet, s)?;
    // Outline grouping: `group_rows:[[first,last],...]`, `group_columns:[...]`.
    if let Some(g) = s.get("group_rows").and_then(Value::as_array) {
        for r in g.iter().filter_map(pair_u64) {
            ws.group_rows(r.0 as u32, r.1 as u32)?;
        }
    }
    if let Some(g) = s.get("group_columns").and_then(Value::as_array) {
        for c in g.iter().filter_map(pair_u64) {
            ws.group_columns(c.0 as u16, c.1 as u16)?;
        }
    }
    // Hide individual rows / columns.
    if let Some(rows) = s.get("hide_rows").and_then(Value::as_array) {
        for r in rows.iter().filter_map(Value::as_u64) {
            ws.set_row_hidden(r as u32)?;
        }
    }
    if let Some(cols) = s.get("hide_columns").and_then(Value::as_array) {
        for c in cols.iter().filter_map(Value::as_u64) {
            ws.set_column_hidden(c as u16)?;
        }
    }
    if s.get("autofit").and_then(flag_of) == Some(true) {
        ws.autofit();
    }
    Ok(())
}

/// A `[a, b]` pair of u64s.
fn pair_u64(v: &Value) -> Option<(u64, u64)> {
    let a = v.as_array()?;
    Some((a.first()?.as_u64()?, a.get(1)?.as_u64()?))
}

/// In-cell sparklines: `sparklines:[{at:[row,col], range:[r1,c1,r2,c2],
/// type?: "line"|"column"|"winloss", markers?, high?, low?}]`. The data range
/// references this sheet.
fn write_xlsx_sparklines(
    ws: &mut rust_xlsxwriter::Worksheet,
    sheet: &str,
    s: &Value,
) -> Result<()> {
    use rust_xlsxwriter::{Sparkline, SparklineType};
    let Some(sparks) = s.get("sparklines").and_then(Value::as_array) else {
        return Ok(());
    };
    for sp in sparks {
        let Some(at) = pair_u64(sp.get("at").unwrap_or(&Value::Null)) else {
            continue;
        };
        let Some(rng) = quad(sp, "range") else {
            continue;
        };
        let stype = match sp.get("type").and_then(Value::as_str).unwrap_or("line") {
            "column" => SparklineType::Column,
            "winloss" | "winlose" => SparklineType::WinLose,
            _ => SparklineType::Line,
        };
        let mut spark = Sparkline::new()
            .set_range((sheet, rng[0], rng[1] as u16, rng[2], rng[3] as u16))
            .set_type(stype);
        if sp.get("markers").and_then(flag_of) == Some(true) {
            spark = spark.show_markers(true);
        }
        if sp.get("high").and_then(flag_of) == Some(true) {
            spark = spark.show_high_point(true);
        }
        if sp.get("low").and_then(flag_of) == Some(true) {
            spark = spark.show_low_point(true);
        }
        ws.add_sparkline(at.0 as u32, at.1 as u16, &spark)?;
    }
    Ok(())
}

/// Insert charts declared on a sheet:
/// `charts:[{type, at:[row,col], title?, series:[{values:[r1,c1,r2,c2],
///           categories?:[...], name?}]}]`. Series ranges reference this sheet.
fn write_xlsx_charts(ws: &mut rust_xlsxwriter::Worksheet, sheet: &str, s: &Value) -> Result<()> {
    use rust_xlsxwriter::{Chart, ChartType};
    let Some(charts) = s.get("charts").and_then(Value::as_array) else {
        return Ok(());
    };
    for ch in charts {
        let ctype = match ch.get("type").and_then(Value::as_str).unwrap_or("column") {
            "bar" => ChartType::Bar,
            "line" => ChartType::Line,
            "pie" => ChartType::Pie,
            "scatter" => ChartType::Scatter,
            "area" => ChartType::Area,
            "doughnut" => ChartType::Doughnut,
            _ => ChartType::Column,
        };
        let mut chart = Chart::new(ctype);
        if let Some(t) = ch.get("title").and_then(Value::as_str) {
            chart.title().set_name(t);
        }
        if let Some(series) = ch.get("series").and_then(Value::as_array) {
            for sv in series {
                let ser = chart.add_series();
                if let Some(v) = sv.get("values").and_then(quad_arr) {
                    ser.set_values((sheet, v[0], v[1] as u16, v[2], v[3] as u16));
                }
                if let Some(c) = sv.get("categories").and_then(quad_arr) {
                    ser.set_categories((sheet, c[0], c[1] as u16, c[2], c[3] as u16));
                }
                if let Some(nm) = sv.get("name").and_then(Value::as_str) {
                    ser.set_name(nm);
                }
            }
        }
        let at = ch.get("at").and_then(Value::as_array);
        let row = at
            .and_then(|a| a.first())
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let col = at
            .and_then(|a| a.get(1))
            .and_then(Value::as_u64)
            .unwrap_or(5) as u16;
        ws.insert_chart(row, col, &chart)?;
    }
    Ok(())
}

/// Write one cell. A scalar writes plainly; a JSON object is a rich cell —
/// `{v|value, f|formula, + styling keys}` — written with a Format.
fn write_xlsx_cell(
    ws: &mut rust_xlsxwriter::Worksheet,
    r: u32,
    c: u16,
    cell: &Value,
) -> Result<()> {
    match cell {
        Value::Null => {}
        Value::Bool(b) => {
            ws.write_boolean(r, c, *b)?;
        }
        Value::Number(n) => {
            ws.write_number(r, c, n.as_f64().unwrap_or(0.0))?;
        }
        Value::Object(o) => {
            // Rich string: `{rich:[{text, +styling}, ...]}` → multiple formats
            // within one cell.
            if let Some(runs) = o.get("rich").and_then(Value::as_array) {
                let parts: Vec<(rust_xlsxwriter::Format, String)> = runs
                    .iter()
                    .filter_map(|r| {
                        let obj = r.as_object()?;
                        let text = obj.get("text").map(cell_to_string).unwrap_or_default();
                        let fmt = xlsx_format(obj).unwrap_or_default();
                        Some((fmt, text))
                    })
                    .collect();
                let refs: Vec<(&rust_xlsxwriter::Format, &str)> =
                    parts.iter().map(|(f, t)| (f, t.as_str())).collect();
                if !refs.is_empty() {
                    ws.write_rich_string(r, c, &refs)?;
                }
                return Ok(());
            }
            if let Some(link) = o.get("link").and_then(Value::as_str) {
                let url = rust_xlsxwriter::Url::new(link);
                match o
                    .get("v")
                    .or_else(|| o.get("value"))
                    .and_then(Value::as_str)
                {
                    Some(text) => {
                        ws.write_url_with_text(r, c, url, text)?;
                    }
                    None => {
                        ws.write_url(r, c, url)?;
                    }
                }
                return Ok(());
            }
            let fmt = xlsx_format(o);
            if let Some(formula) = o
                .get("f")
                .or_else(|| o.get("formula"))
                .and_then(Value::as_str)
            {
                match &fmt {
                    Some(f) => {
                        ws.write_formula_with_format(r, c, formula, f)?;
                    }
                    None => {
                        ws.write_formula(r, c, formula)?;
                    }
                }
            } else {
                let v = o
                    .get("v")
                    .or_else(|| o.get("value"))
                    .unwrap_or(&Value::Null);
                match (v, &fmt) {
                    (Value::Number(n), Some(f)) => {
                        ws.write_number_with_format(r, c, n.as_f64().unwrap_or(0.0), f)?;
                    }
                    (Value::Number(n), None) => {
                        ws.write_number(r, c, n.as_f64().unwrap_or(0.0))?;
                    }
                    (Value::Bool(b), Some(f)) => {
                        ws.write_boolean_with_format(r, c, *b, f)?;
                    }
                    (Value::Bool(b), None) => {
                        ws.write_boolean(r, c, *b)?;
                    }
                    (Value::Null, _) => {}
                    (other, Some(f)) => {
                        ws.write_string_with_format(r, c, cell_to_string(other), f)?;
                    }
                    (other, None) => {
                        ws.write_string(r, c, cell_to_string(other))?;
                    }
                }
            }
        }
        other => {
            ws.write_string(r, c, cell_to_string(other))?;
        }
    }
    Ok(())
}

fn write_ods(path: &str, sheets: &[(String, Vec<Vec<Value>>)]) -> Result<()> {
    use lo_core::{CellAddr, CellValue, Workbook};
    let mut wb = Workbook::new("stryke-office");
    wb.sheets.clear(); // drop the default "Sheet1" lo_core seeds
    for (name, rows) in sheets {
        let sheet = wb.ensure_sheet(name);
        for (r, row) in rows.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                let v = match cell {
                    Value::Null => CellValue::Empty,
                    Value::Bool(b) => CellValue::Bool(*b),
                    Value::Number(n) => CellValue::Number(n.as_f64().unwrap_or(0.0)),
                    other => CellValue::Text(cell_to_string(other)),
                };
                sheet.set(CellAddr::new(r as u32, c as u32), v);
            }
        }
    }
    lo_odf::save_spreadsheet_document(path, &wb)?;
    Ok(())
}

fn op_sheet_write(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?.to_string();
    let n = match target_ext(&opts, &path).as_str() {
        "xlsx" => {
            let raw = opts
                .get("sheets")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("missing sheets (expected array)"))?;
            write_xlsx(&path, raw, &opts)?;
            raw.len()
        }
        "ods" => {
            let sheets = json_sheets(&opts)?;
            write_ods(&path, &sheets)?;
            sheets.len()
        }
        "csv" | "tsv" => {
            let sheets = json_sheets(&opts)?;
            let delim = opts
                .get("delimiter")
                .and_then(Value::as_str)
                .and_then(|s| s.chars().next())
                .unwrap_or(if target_ext(&opts, &path) == "tsv" {
                    '\t'
                } else {
                    ','
                });
            write_csv(&path, &sheets, delim)?;
            sheets.len()
        }
        "html" | "htm" => {
            let sheets = json_sheets(&opts)?;
            write_sheet_html(&path, &sheets)?;
            sheets.len()
        }
        "md" | "markdown" => {
            let sheets = json_sheets(&opts)?;
            write_sheet_md(&path, &sheets)?;
            sheets.len()
        }
        other => return Err(anyhow!("unsupported spreadsheet write format: {other}")),
    };
    Ok(json!({"ok": true, "path": path, "sheets": n}))
}

/// Combine several spreadsheets into one. opts: inputs => [paths],
/// output => path, mode => "sheets" (default; every source sheet is carried
/// over, names de-duplicated) | "rows" (all rows stacked into one "Merged"
/// sheet — the right mode for csv/tsv targets), format => override. Doubles as
/// conversion via the output extension. Returns `{ ok, path, sources, sheets }`.
fn op_sheet_merge(opts: Value) -> Result<Value> {
    let inputs = opts
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing inputs (expected array of paths)"))?;
    let output = req_str(&opts, "output")?.to_string();
    let rows_mode = opts.get("mode").and_then(Value::as_str) == Some("rows");

    let mut sheets: Vec<Value> = Vec::new();
    let mut combined: Vec<Value> = Vec::new();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for inp in inputs {
        let path = inp
            .as_str()
            .ok_or_else(|| anyhow!("input path must be a string"))?;
        let read = op_sheet_read(json!({ "path": path }))?;
        let src = read
            .get("sheets")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for s in src {
            if rows_mode {
                if let Some(rs) = s.get("rows").and_then(Value::as_array) {
                    combined.extend(rs.iter().cloned());
                }
            } else {
                let base = s.get("name").and_then(Value::as_str).unwrap_or("Sheet");
                let mut name = base.to_string();
                let mut k = 2;
                while used.contains(&name) {
                    name = format!("{base} ({k})");
                    k += 1;
                }
                used.insert(name.clone());
                let rows = s.get("rows").cloned().unwrap_or_else(|| json!([]));
                sheets.push(json!({ "name": name, "rows": rows }));
            }
        }
    }
    if rows_mode {
        sheets.push(json!({ "name": "Merged", "rows": combined }));
    }

    let n = sheets.len();
    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "sources": inputs.len(), "sheets": n }))
}

/// Concatenate spreadsheets aligned by column name (SQL UNION / pandas concat).
/// Each input is read as records and stacked; the output columns are the union
/// of all field names (first-seen order), missing values null-filled — so files
/// with the same logical columns in different orders combine correctly. opts:
/// inputs => [paths], output, sheet => source sheet selector, format. Returns
/// `{ ok, path, sources, rows, fields }`.
fn op_sheet_union(opts: Value) -> Result<Value> {
    let inputs = opts
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing inputs (expected array of paths)"))?;
    let output = req_str(&opts, "output")?.to_string();

    let mut all: Vec<Value> = Vec::new();
    for inp in inputs {
        let p = inp
            .as_str()
            .ok_or_else(|| anyhow!("input path must be a string"))?;
        let recs = op_sheet_records(json!({ "path": p, "sheet": opts.get("sheet") }))?;
        if let Some(a) = recs.get("records").and_then(Value::as_array) {
            all.extend(a.iter().cloned());
        }
    }

    let mut wopts = json!({ "path": output, "records": all });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    let res = op_records_write(wopts)?;
    Ok(json!({
        "ok": true,
        "path": output,
        "sources": inputs.len(),
        "rows": res.get("rows").cloned().unwrap_or(json!(0)),
        "fields": res.get("fields").cloned().unwrap_or(json!([])),
    }))
}

/// A cell's numeric value, if it is a number or a numeric-looking string.
fn sheet_cell_num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.parse().ok()).flatten()
        }
        _ => None,
    }
}

/// Whether a cell is blank (null or empty/whitespace text).
fn sheet_cell_blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        _ => false,
    }
}

/// Per-column descriptive statistics for one sheet. opts: path, sheet => name or
/// 0-based index (default: first), header => bool (first row is column names;
/// default true). Returns `{ sheet, rows, columns: [{ name, count, numeric,
/// blanks, sum?, min?, max?, mean? }] }` — sum/min/max/mean only when the column
/// has numeric cells.
fn op_sheet_stats(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let name = sheet["name"].as_str().unwrap_or("").to_string();
    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let ncols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);
    let header_row = if header { rows.first() } else { None };
    let data = if header && !rows.is_empty() {
        &rows[1..]
    } else {
        &rows[..]
    };

    let mut columns = Vec::with_capacity(ncols);
    for c in 0..ncols {
        let cname = header_row
            .and_then(|hr| hr.as_array())
            .and_then(|a| a.get(c))
            .map(cell_to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("Col{}", c + 1));
        let (mut count, mut numeric, mut blanks) = (0u64, 0u64, 0u64);
        let (mut sum, mut min, mut max) = (0f64, f64::INFINITY, f64::NEG_INFINITY);
        for row in data {
            let cell = row
                .as_array()
                .and_then(|a| a.get(c))
                .unwrap_or(&Value::Null);
            if sheet_cell_blank(cell) {
                blanks += 1;
                continue;
            }
            count += 1;
            if let Some(x) = sheet_cell_num(cell) {
                numeric += 1;
                sum += x;
                min = min.min(x);
                max = max.max(x);
            }
        }
        let mut col =
            json!({ "name": cname, "count": count, "numeric": numeric, "blanks": blanks });
        if numeric > 0 {
            col["sum"] = json!(sum);
            col["min"] = json!(min);
            col["max"] = json!(max);
            col["mean"] = json!(sum / numeric as f64);
        }
        columns.push(col);
    }
    Ok(json!({ "sheet": name, "rows": data.len(), "columns": columns }))
}

/// Linear-interpolated percentile of a pre-sorted slice (pandas default method).
/// q in 0.0..=1.0. Empty slice → 0.0.
fn percentile_sorted(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = q * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = rank - lo as f64;
    sorted[lo] + (sorted[hi] - sorted[lo]) * frac
}

/// Pandas-style numeric summary: per numeric column, count/mean/std/min/25%/
/// 50%/75%/max. Distinct from `sheet_stats` (which adds blanks/sum but no
/// std-dev, median, or quartiles). opts: path, sheet, header (default true).
/// `std` is the sample standard deviation (ddof=1; 0 when count < 2). Only
/// columns with at least one numeric value are reported. Returns `{ sheet, rows,
/// columns: [{ name, count, mean, std, min, p25, p50, p75, max }] }`.
fn op_sheet_describe(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let name = sheet["name"].as_str().unwrap_or("").to_string();
    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let ncols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);
    let header_row = if header { rows.first() } else { None };
    let data = if header && !rows.is_empty() {
        &rows[1..]
    } else {
        &rows[..]
    };

    let mut columns = Vec::new();
    for c in 0..ncols {
        let mut vals: Vec<f64> = data
            .iter()
            .filter_map(|row| row.as_array().and_then(|a| a.get(c)))
            .filter_map(sheet_cell_num)
            .collect();
        if vals.is_empty() {
            continue;
        }
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let cname = header_row
            .and_then(|hr| hr.as_array())
            .and_then(|a| a.get(c))
            .map(cell_to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("Col{}", c + 1));
        let n = vals.len();
        let mean = vals.iter().sum::<f64>() / n as f64;
        let std = if n > 1 {
            (vals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
        } else {
            0.0
        };
        columns.push(json!({
            "name": cname,
            "count": n,
            "mean": mean,
            "std": std,
            "min": vals[0],
            "p25": percentile_sorted(&vals, 0.25),
            "p50": percentile_sorted(&vals, 0.50),
            "p75": percentile_sorted(&vals, 0.75),
            "max": vals[n - 1],
        }));
    }
    Ok(json!({ "sheet": name, "rows": data.len(), "columns": columns }))
}

/// Compute an arbitrary percentile of a numeric column (generalizes the fixed
/// quartiles in `sheet_describe`). opts: path, column => name or 0-based index
/// (required), q => quantile in 0.0..=1.0 (required; e.g. 0.9 for p90), sheet,
/// header (default true). Linear interpolation (pandas default). Returns
/// `{ column, q, value, count }` (value is null when the column has no numbers).
fn op_sheet_quantile(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let q = opts
        .get("q")
        .and_then(Value::as_f64)
        .filter(|x| (0.0..=1.0).contains(x))
        .ok_or_else(|| anyhow!("missing q (a quantile in 0.0..=1.0)"))?;

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;
    let col_name = header_row
        .and_then(|hr| hr.get(col))
        .map(cell_to_string)
        .unwrap_or_else(|| format!("Col{}", col + 1));

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut vals: Vec<f64> = rows[data_start..]
        .iter()
        .filter_map(|r| {
            r.as_array()
                .and_then(|a| a.get(col))
                .and_then(sheet_cell_num)
        })
        .collect();
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let value = if vals.is_empty() {
        Value::Null
    } else {
        json!(percentile_sorted(&vals, q))
    };
    Ok(json!({ "column": col_name, "q": q, "value": value, "count": vals.len() }))
}

/// Pearson correlation coefficient over paired observations. Returns None when
/// fewer than two points or either side has zero variance.
fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let n = xs.len();
    if n < 2 {
        return None;
    }
    let mx = xs.iter().sum::<f64>() / n as f64;
    let my = ys.iter().sum::<f64>() / n as f64;
    let mut cov = 0.0;
    let mut sx = 0.0;
    let mut sy = 0.0;
    for (x, y) in xs.iter().zip(ys) {
        cov += (x - mx) * (y - my);
        sx += (x - mx).powi(2);
        sy += (y - my).powi(2);
    }
    if sx == 0.0 || sy == 0.0 {
        return None;
    }
    Some(cov / (sx.sqrt() * sy.sqrt()))
}

/// Pearson correlation matrix between numeric columns (the analogue of pandas
/// `df.corr()`). opts: path, sheet, header (default true). Each pair is computed
/// over rows where both columns are numeric (pairwise-complete); the diagonal is
/// 1.0 and undefined cells (fewer than two shared points or zero variance) are
/// `null`. Only columns with at least one numeric value are included. Returns
/// `{ sheet, columns: [name], matrix: [[r]] }`.
fn op_sheet_corr(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let name = sheet["name"].as_str().unwrap_or("").to_string();
    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let ncols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);
    let header_row = if header { rows.first() } else { None };
    let data = if header && !rows.is_empty() {
        &rows[1..]
    } else {
        &rows[..]
    };

    // Per-column aligned numeric values (None where the cell is non-numeric).
    let mut cols: Vec<(String, Vec<Option<f64>>)> = Vec::new();
    for c in 0..ncols {
        let vals: Vec<Option<f64>> = data
            .iter()
            .map(|row| {
                row.as_array()
                    .and_then(|a| a.get(c))
                    .and_then(sheet_cell_num)
            })
            .collect();
        if !vals.iter().any(Option::is_some) {
            continue;
        }
        let cname = header_row
            .and_then(|hr| hr.as_array())
            .and_then(|a| a.get(c))
            .map(cell_to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("Col{}", c + 1));
        cols.push((cname, vals));
    }

    let names: Vec<Value> = cols.iter().map(|(n, _)| json!(n)).collect();
    let mut matrix = Vec::with_capacity(cols.len());
    for (i, (_, xi)) in cols.iter().enumerate() {
        let mut rowv = Vec::with_capacity(cols.len());
        for (j, (_, xj)) in cols.iter().enumerate() {
            if i == j {
                rowv.push(json!(1.0));
                continue;
            }
            let (mut xs, mut ys) = (Vec::new(), Vec::new());
            for (a, b) in xi.iter().zip(xj) {
                if let (Some(a), Some(b)) = (a, b) {
                    xs.push(*a);
                    ys.push(*b);
                }
            }
            rowv.push(pearson(&xs, &ys).map_or(Value::Null, |r| json!(r)));
        }
        matrix.push(Value::Array(rowv));
    }
    Ok(json!({ "sheet": name, "columns": names, "matrix": matrix }))
}

/// Escape a cell for a GitHub-flavored Markdown table: literal `|` is escaped
/// and newlines collapse to spaces (a table cell cannot span lines).
fn md_cell_escape(s: &str) -> String {
    s.replace('|', "\\|")
        .replace(['\n', '\r'], " ")
        .trim()
        .to_string()
}

/// Render a spreadsheet as a GitHub-flavored Markdown table. opts: path, output
/// => write to a `.md` file (omit to return the text), sheet => selector, header
/// => first row is the header (default true; when false, generic `Col1..` headers
/// are synthesized). Whole-number floats render without a trailing `.0`. Returns
/// `{ ok, rows, cols, markdown }` (plus `path` when written).
fn op_sheet_to_md(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let ncols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);
    if ncols == 0 {
        return Err(anyhow!("sheet has no columns"));
    }

    let cell_text = |c: &Value| -> String {
        let raw = match c.as_f64() {
            Some(x) if c.is_number() && x.fract() == 0.0 && x.is_finite() => (x as i64).to_string(),
            _ => cell_to_string(c),
        };
        md_cell_escape(&raw)
    };
    let fmt_row = |row: &Value| -> String {
        let cells: Vec<String> = (0..ncols)
            .map(|c| {
                let v = row
                    .as_array()
                    .and_then(|a| a.get(c))
                    .unwrap_or(&Value::Null);
                cell_text(v)
            })
            .collect();
        format!("| {} |", cells.join(" | "))
    };

    let sep = format!("| {} |", vec!["---"; ncols].join(" | "));
    let mut lines: Vec<String> = Vec::new();
    let body: &[Value] = if header && !rows.is_empty() {
        lines.push(fmt_row(&rows[0]));
        lines.push(sep);
        &rows[1..]
    } else {
        let hdr: Vec<String> = (1..=ncols).map(|i| format!("Col{i}")).collect();
        lines.push(format!("| {} |", hdr.join(" | ")));
        lines.push(sep);
        &rows[..]
    };
    for row in body {
        lines.push(fmt_row(row));
    }
    let markdown = format!("{}\n", lines.join("\n"));

    let mut out = json!({ "ok": true, "rows": rows.len(), "cols": ncols, "markdown": markdown });
    if let Some(output) = opts.get("output").and_then(Value::as_str) {
        std::fs::write(output, &markdown)?;
        out["path"] = json!(output);
    }
    Ok(out)
}

/// Split one Markdown table row into trimmed cells, honoring `\|` escapes.
fn split_md_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '|' => {
                cells.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

/// True if a row is a GFM header/body delimiter (every cell is `:?-+:?`).
fn is_md_separator(cells: &[String]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|c| {
            let t = c.trim();
            !t.is_empty() && t.contains('-') && t.chars().all(|ch| ch == '-' || ch == ':')
        })
}

/// Parse a GitHub-flavored Markdown table into a spreadsheet file (the inverse of
/// `sheet_to_md`). opts: markdown => table text, or path => a `.md` file to read;
/// output (required) => xlsx/ods/csv; name => sheet name (default "Sheet1");
/// format => output override. The first contiguous run of `|`-delimited lines is
/// taken as the table and its delimiter row dropped. Returns `{ ok, path, rows,
/// cols }`.
fn op_md_to_sheet(opts: Value) -> Result<Value> {
    let text = match opts.get("markdown").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => {
            let path = req_str(&opts, "path")?;
            String::from_utf8_lossy(&std::fs::read(path)?).into_owned()
        }
    };
    let output = req_str(&opts, "output")?.to_string();
    let name = opts
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Sheet1")
        .to_string();

    // Take the first contiguous block of pipe-delimited lines as the table.
    let mut block: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim().starts_with('|') {
            block.push(line);
        } else if !block.is_empty() {
            break;
        }
    }
    if block.is_empty() {
        return Err(anyhow!("no Markdown table found (no leading-pipe rows)"));
    }

    let rows: Vec<Value> = block
        .iter()
        .map(|l| split_md_row(l))
        .filter(|cells| !is_md_separator(cells))
        .map(|cells| Value::Array(cells.into_iter().map(Value::String).collect()))
        .collect();
    let cols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);

    let mut wopts = json!({ "path": output, "sheets": [{ "name": name, "rows": rows }] });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "rows": rows.len(), "cols": cols }))
}

/// Render a spreadsheet as an HTML table (for web reports / HTML email). opts:
/// path, output => write to an `.html` file (omit to return the markup), sheet =>
/// selector, header => first row becomes `<thead><th>` cells (default true),
/// title => optional `<h2>` caption, full => wrap in a complete `<html>` document
/// (default false = bare `<table>`). Cells are HTML-escaped; whole-number floats
/// render without a trailing `.0`. Returns `{ ok, rows, cols, html, path? }`.
fn op_sheet_to_html(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let full = opts.get("full").and_then(flag_of).unwrap_or(false);
    let ncols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);

    let cell_html = |c: &Value| -> String {
        let raw = match c.as_f64() {
            Some(x) if c.is_number() && x.fract() == 0.0 && x.is_finite() => (x as i64).to_string(),
            _ => cell_to_string(c),
        };
        xml_escape(&raw)
    };
    let row_cells = |row: &Value, tag: &str| -> String {
        let cells: Vec<String> = (0..ncols)
            .map(|c| {
                let v = row
                    .as_array()
                    .and_then(|a| a.get(c))
                    .unwrap_or(&Value::Null);
                format!("<{tag}>{}</{tag}>", cell_html(v))
            })
            .collect();
        format!("    <tr>{}</tr>", cells.join(""))
    };

    let mut body = String::new();
    if let Some(title) = opts.get("title").and_then(Value::as_str) {
        body.push_str(&format!("<h2>{}</h2>\n", xml_escape(title)));
    }
    body.push_str("<table>\n");
    let data: &[Value] = if header && !rows.is_empty() {
        body.push_str("  <thead>\n");
        body.push_str(&row_cells(&rows[0], "th"));
        body.push_str("\n  </thead>\n");
        &rows[1..]
    } else {
        &rows[..]
    };
    body.push_str("  <tbody>\n");
    for row in data {
        body.push_str(&row_cells(row, "td"));
        body.push('\n');
    }
    body.push_str("  </tbody>\n</table>\n");

    let html = if full {
        format!(
            "<!DOCTYPE html>\n<html>\n<head><meta charset=\"utf-8\">\n<style>table{{border-collapse:collapse}}th,td{{border:1px solid #ccc;padding:4px 8px}}</style>\n</head>\n<body>\n{body}</body>\n</html>\n"
        )
    } else {
        body
    };

    let mut out = json!({ "ok": true, "rows": rows.len(), "cols": ncols, "html": html });
    if let Some(output) = opts.get("output").and_then(Value::as_str) {
        std::fs::write(output, &html)?;
        out["path"] = json!(output);
    }
    Ok(out)
}

/// Render a sheet as an aligned plain-text table (terminal-friendly). opts:
/// path, output => write to a file (omit to return the text), sheet => selector,
/// header => underline the first row with dashes (default true), border => draw
/// an ASCII grid with `+`/`-`/`|` (default false = space-aligned columns).
/// Columns are padded to their widest cell; whole-number floats lose the
/// trailing `.0`. Returns `{ ok, rows, cols, text, path? }`.
fn op_sheet_to_text(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let border = opts.get("border").and_then(flag_of).unwrap_or(false);
    let ncols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);

    let cell_text = |c: &Value| -> String {
        match c.as_f64() {
            Some(x) if c.is_number() && x.fract() == 0.0 && x.is_finite() => (x as i64).to_string(),
            _ => cell_to_string(c),
        }
    };
    // Stringify the whole grid and measure each column's display width (chars).
    let grid: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            (0..ncols)
                .map(|c| {
                    cell_text(
                        row.as_array()
                            .and_then(|a| a.get(c))
                            .unwrap_or(&Value::Null),
                    )
                })
                .collect()
        })
        .collect();
    let widths: Vec<usize> = (0..ncols)
        .map(|c| grid.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    let pad = |s: &str, w: usize| -> String {
        let mut out = s.to_string();
        out.push_str(&" ".repeat(w.saturating_sub(s.chars().count())));
        out
    };

    let mut lines: Vec<String> = Vec::new();
    if border {
        let rule = format!(
            "+{}+",
            widths
                .iter()
                .map(|&w| "-".repeat(w + 2))
                .collect::<Vec<_>>()
                .join("+")
        );
        let fmt_row = |r: &[String]| {
            format!(
                "| {} |",
                r.iter()
                    .enumerate()
                    .map(|(c, cell)| pad(cell, widths[c]))
                    .collect::<Vec<_>>()
                    .join(" | ")
            )
        };
        lines.push(rule.clone());
        for (i, r) in grid.iter().enumerate() {
            lines.push(fmt_row(r));
            if i == 0 && header {
                lines.push(rule.clone());
            }
        }
        lines.push(rule);
    } else {
        let fmt_row = |r: &[String]| {
            r.iter()
                .enumerate()
                .map(|(c, cell)| pad(cell, widths[c]))
                .collect::<Vec<_>>()
                .join("  ")
        };
        for (i, r) in grid.iter().enumerate() {
            lines.push(fmt_row(r));
            if i == 0 && header {
                lines.push(
                    widths
                        .iter()
                        .map(|&w| "-".repeat(w))
                        .collect::<Vec<_>>()
                        .join("  "),
                );
            }
        }
    }
    let text = format!("{}\n", lines.join("\n"));

    let mut out = json!({ "ok": true, "rows": rows.len(), "cols": ncols, "text": text });
    if let Some(output) = opts.get("output").and_then(Value::as_str) {
        std::fs::write(output, &text)?;
        out["path"] = json!(output);
    }
    Ok(out)
}

/// Parse an A1 cell reference (e.g. `B2`, `AA10`) into 0-based `(row, col)`.
/// Row 1 / column A map to `(0, 0)`. Returns None for malformed input.
fn parse_a1(s: &str) -> Option<(usize, usize)> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = s.split_at(split);
    if letters.is_empty() || digits.is_empty() {
        return None;
    }
    let mut col = 0usize;
    for c in letters.chars() {
        if !c.is_ascii_alphabetic() {
            return None;
        }
        col = col * 26 + (c.to_ascii_uppercase() as usize - 'A' as usize + 1);
    }
    let row: usize = digits.parse().ok()?;
    if col == 0 || row == 0 {
        return None;
    }
    Some((row - 1, col - 1))
}

/// Read a single cell by A1 reference. opts: path, cell (e.g. "B2", required),
/// sheet => name/index. Returns `{ cell, row, col, value }` (0-based row/col;
/// value is null if out of range).
fn op_sheet_get_cell(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let cell = req_str(&opts, "cell")?;
    let (r, c) = parse_a1(cell).ok_or_else(|| anyhow!("bad A1 reference: {cell}"))?;
    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;
    let value = sheet["rows"]
        .as_array()
        .and_then(|rows| rows.get(r))
        .and_then(Value::as_array)
        .and_then(|row| row.get(c))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({ "cell": cell, "row": r, "col": c, "value": value }))
}

/// Set a single cell by A1 reference, growing the grid with blanks as needed.
/// opts: path, cell (required), value (default null/blank), output (default in
/// place), sheet => name/index, format. Returns `{ ok, path, cell }`.
fn op_sheet_set_cell(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let cell = req_str(&opts, "cell")?;
    let (r, c) = parse_a1(cell).ok_or_else(|| anyhow!("bad A1 reference: {cell}"))?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let value = opts.get("value").cloned().unwrap_or(Value::Null);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if sheets.is_empty() {
        sheets.push(json!({ "name": "Sheet1", "rows": [] }));
    }
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let mut rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if rows.len() <= r {
        rows.resize(r + 1, Value::Array(Vec::new()));
    }
    let mut row = rows[r].as_array().cloned().unwrap_or_default();
    if row.len() <= c {
        row.resize(c + 1, Value::Null);
    }
    row[c] = value;
    rows[r] = Value::Array(row);
    sheets[target]["rows"] = Value::Array(rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "cell": cell }))
}

/// Read a rectangular A1 range (e.g. "A1:C3") as a subgrid. opts: path, range
/// (required; a single cell like "B2" reads a 1×1 range), sheet => name/index.
/// Out-of-bounds cells come back as null. Returns `{ range, nrows, ncols, rows }`.
fn op_sheet_get_range(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let range = req_str(&opts, "range")?;
    let (a, b) = match range.split_once(':') {
        Some((l, r)) => (
            parse_a1(l).ok_or_else(|| anyhow!("bad A1 reference: {l}"))?,
            parse_a1(r).ok_or_else(|| anyhow!("bad A1 reference: {r}"))?,
        ),
        None => {
            let p = parse_a1(range).ok_or_else(|| anyhow!("bad A1 reference: {range}"))?;
            (p, p)
        }
    };
    let (r0, r1) = (a.0.min(b.0), a.0.max(b.0));
    let (c0, c1) = (a.1.min(b.1), a.1.max(b.1));

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;
    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);

    let sub: Vec<Value> = (r0..=r1)
        .map(|r| {
            let row = rows.get(r).and_then(Value::as_array);
            let cells: Vec<Value> = (c0..=c1)
                .map(|c| row.and_then(|a| a.get(c)).cloned().unwrap_or(Value::Null))
                .collect();
            Value::Array(cells)
        })
        .collect();
    Ok(json!({ "range": range, "nrows": r1 - r0 + 1, "ncols": c1 - c0 + 1, "rows": sub }))
}

/// Paste a 2D block of values at a top-left A1 cell, growing the grid as needed
/// (the bulk analogue of `sheet_set_cell`). opts: path, cell => top-left (e.g.
/// "B2", required), values => array of rows (required), output (default in
/// place), sheet => name/index, format. Returns `{ ok, path, cells }` (cells
/// written). Existing cells outside the block are untouched.
fn op_sheet_set_range(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let cell = req_str(&opts, "cell")?;
    let (r0, c0) = parse_a1(cell).ok_or_else(|| anyhow!("bad A1 reference: {cell}"))?;
    let values = opts
        .get("values")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing values (expected array of rows)"))?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if sheets.is_empty() {
        sheets.push(json!({ "name": "Sheet1", "rows": [] }));
    }
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let mut rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut written = 0u64;
    for (i, vrow) in values.iter().enumerate() {
        let Some(vcells) = vrow.as_array() else {
            continue;
        };
        let rr = r0 + i;
        if rows.len() <= rr {
            rows.resize(rr + 1, Value::Array(Vec::new()));
        }
        let mut row = rows[rr].as_array().cloned().unwrap_or_default();
        for (j, val) in vcells.iter().enumerate() {
            let cc = c0 + j;
            if row.len() <= cc {
                row.resize(cc + 1, Value::Null);
            }
            row[cc] = val.clone();
            written += 1;
        }
        rows[rr] = Value::Array(row);
    }
    sheets[target]["rows"] = Value::Array(rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "cells": written }))
}

/// Select the target sheet index from `opts["sheet"]` (name/index/first),
/// inserting a default empty sheet if the workbook has none.
fn sheet_target_index(opts: &Value, sheets: &mut Vec<Value>) -> Result<usize> {
    if sheets.is_empty() {
        sheets.push(json!({ "name": "Sheet1", "rows": [] }));
    }
    match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))
}

/// Insert blank rows into a sheet at a 1-based position, shifting later rows
/// down. opts: path, at => 1-based row to insert before (default 1; clamped to
/// the end), count => number of rows (default 1), output (default in place),
/// sheet, format. Returns `{ ok, path, inserted }`.
fn op_sheet_insert_rows(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let at = opts.get("at").and_then(Value::as_u64).unwrap_or(1).max(1) as usize - 1;
    let count = opts.get("count").and_then(Value::as_u64).unwrap_or(1) as usize;

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let mut rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let pos = at.min(rows.len());
    for _ in 0..count {
        rows.insert(pos, Value::Array(Vec::new()));
    }
    sheets[target]["rows"] = Value::Array(rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "inserted": count }))
}

/// Delete rows from a sheet starting at a 1-based position. opts: path, at =>
/// 1-based first row to delete (required), count => number of rows (default 1),
/// output (default in place), sheet, format. Returns `{ ok, path, deleted }`.
fn op_sheet_delete_rows(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let at = opts
        .get("at")
        .and_then(Value::as_u64)
        .filter(|&n| n >= 1)
        .ok_or_else(|| anyhow!("missing at (1-based row number)"))? as usize
        - 1;
    let count = opts.get("count").and_then(Value::as_u64).unwrap_or(1) as usize;

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let mut rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let deleted = if at < rows.len() {
        let end = (at + count).min(rows.len());
        rows.drain(at..end).count()
    } else {
        0
    };
    sheets[target]["rows"] = Value::Array(rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "deleted": deleted }))
}

/// Insert a column at a 1-based position, shifting later columns right (the
/// column analogue of `sheet_insert_rows`; `sheet_add_column` only appends).
/// opts: path, at => 1-based column to insert before (default 1; clamped to each
/// row's width), name => header for the new column (header row only), value =>
/// fill for data rows (default blank), output (default in place), sheet, header
/// (default true), format. Returns `{ ok, path, at }`.
fn op_sheet_insert_column(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let at = opts.get("at").and_then(Value::as_u64).unwrap_or(1).max(1) as usize - 1;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let name = opts.get("name").and_then(Value::as_str).unwrap_or("");
    let fill = opts.get("value").cloned().unwrap_or(Value::Null);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let new_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let cells = row.as_array().cloned().unwrap_or_default();
            let pos = at.min(cells.len());
            let inserted = if header && i == 0 {
                json!(name)
            } else {
                fill.clone()
            };
            let mut out: Vec<Value> = cells[..pos].to_vec();
            out.push(inserted);
            out.extend_from_slice(&cells[pos..]);
            Value::Array(out)
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "at": at + 1 }))
}

/// Append a running-total (cumulative sum) column for a numeric column. opts:
/// path, output, column => name or 0-based index (required), into => new column
/// header (default "{column}_cumsum"), sheet, header (default true), format.
/// Each data row gets the running sum so far (non-numeric cells leave the total
/// unchanged). Returns `{ ok, path, column }` (the new column's header).
fn op_sheet_cumsum(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;
    let base = header_row
        .and_then(|hr| hr.get(col))
        .map(cell_to_string)
        .unwrap_or_else(|| format!("Col{}", col + 1));
    let into = opts
        .get("into")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{base}_cumsum"));

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut running = 0f64;
    let new_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut cells = row.as_array().cloned().unwrap_or_default();
            if i < data_start {
                cells.push(json!(into));
            } else {
                if let Some(x) = cells.get(col).and_then(sheet_cell_num) {
                    running += x;
                }
                cells.push(json!(running));
            }
            Value::Array(cells)
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "column": into }))
}

/// Append a percent-of-total column: each numeric value as a percentage of the
/// column's sum. opts: path, output, column => name or 0-based index (required),
/// into => new column header (default "{column}_pct"), decimals => round to this
/// many places (default: full precision), sheet, header (default true), format.
/// Non-numeric cells get a blank. Returns `{ ok, path, column }`.
fn op_sheet_pct(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;
    let base = header_row
        .and_then(|hr| hr.get(col))
        .map(cell_to_string)
        .unwrap_or_else(|| format!("Col{}", col + 1));
    let into = opts
        .get("into")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{base}_pct"));
    let decimals = opts.get("decimals").and_then(Value::as_i64);

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let total: f64 = rows[data_start..]
        .iter()
        .filter_map(|r| {
            r.as_array()
                .and_then(|a| a.get(col))
                .and_then(sheet_cell_num)
        })
        .sum();

    let new_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut cells = row.as_array().cloned().unwrap_or_default();
            if i < data_start {
                cells.push(json!(into));
            } else {
                let cell = match cells.get(col).and_then(sheet_cell_num) {
                    Some(x) if total != 0.0 => {
                        let pct = x / total * 100.0;
                        let v = match decimals {
                            Some(d) => {
                                let f = 10f64.powi(d as i32);
                                (pct * f).round() / f
                            }
                            None => pct,
                        };
                        json!(v)
                    }
                    _ => json!(""),
                };
                cells.push(cell);
            }
            Value::Array(cells)
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "column": into }))
}

/// Append a normalized copy of a numeric column. opts: path, output, column =>
/// name or 0-based index (required), method => "minmax" (default; scale to
/// 0..1 via (x-min)/(max-min)) or "zscore" ((x-mean)/population-std), into =>
/// new column header (default "{column}_norm"), decimals => round, sheet, header
/// (default true), format. Degenerate spreads (max==min / std==0) map to 0;
/// non-numeric cells get a blank. Returns `{ ok, path, column }`.
fn op_sheet_normalize(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let method = opts
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("minmax");
    if !matches!(method, "minmax" | "zscore") {
        return Err(anyhow!("unknown method: {method} (minmax|zscore)"));
    }

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;
    let base = header_row
        .and_then(|hr| hr.get(col))
        .map(cell_to_string)
        .unwrap_or_else(|| format!("Col{}", col + 1));
    let into = opts
        .get("into")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{base}_norm"));
    let decimals = opts.get("decimals").and_then(Value::as_i64);

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let vals: Vec<f64> = rows[data_start..]
        .iter()
        .filter_map(|r| {
            r.as_array()
                .and_then(|a| a.get(col))
                .and_then(sheet_cell_num)
        })
        .collect();
    let n = vals.len();
    let (min, max) = vals
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &x| {
            (lo.min(x), hi.max(x))
        });
    let mean = if n > 0 {
        vals.iter().sum::<f64>() / n as f64
    } else {
        0.0
    };
    let std = if n > 0 {
        (vals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64).sqrt()
    } else {
        0.0
    };

    let scale = |x: f64| -> f64 {
        let v = match method {
            "zscore" => {
                if std == 0.0 {
                    0.0
                } else {
                    (x - mean) / std
                }
            }
            _ => {
                if max == min {
                    0.0
                } else {
                    (x - min) / (max - min)
                }
            }
        };
        match decimals {
            Some(d) => {
                let f = 10f64.powi(d as i32);
                (v * f).round() / f
            }
            None => v,
        }
    };

    let new_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut cells = row.as_array().cloned().unwrap_or_default();
            if i < data_start {
                cells.push(json!(into));
            } else {
                let cell = match cells.get(col).and_then(sheet_cell_num) {
                    Some(x) => json!(scale(x)),
                    None => json!(""),
                };
                cells.push(cell);
            }
            Value::Array(cells)
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "column": into }))
}

/// Append a moving-average (rolling mean) column over a fixed window. opts: path,
/// output, column => name or 0-based index (required), window => number of rows
/// (required, ≥1), into => new column header (default "{column}_ma{window}"),
/// decimals => round, sheet, header (default true), format. Each row's value is
/// the mean of the current and previous `window-1` cells; rows before the window
/// fills, or where any cell in the window is non-numeric, get a blank. Returns
/// `{ ok, path, column }`.
fn op_sheet_movavg(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let window = opts
        .get("window")
        .and_then(Value::as_u64)
        .filter(|&w| w >= 1)
        .ok_or_else(|| anyhow!("missing window (>= 1)"))? as usize;

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;
    let base = header_row
        .and_then(|hr| hr.get(col))
        .map(cell_to_string)
        .unwrap_or_else(|| format!("Col{}", col + 1));
    let into = opts
        .get("into")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{base}_ma{window}"));
    let decimals = opts.get("decimals").and_then(Value::as_i64);

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    // Numeric value at each data-row index (None if blank/non-numeric).
    let nums: Vec<Option<f64>> = rows[data_start..]
        .iter()
        .map(|r| {
            r.as_array()
                .and_then(|a| a.get(col))
                .and_then(sheet_cell_num)
        })
        .collect();

    let mut new_rows: Vec<Value> = Vec::with_capacity(rows.len());
    if data_start == 1 {
        let mut hr = rows[0].as_array().cloned().unwrap_or_default();
        hr.push(json!(into));
        new_rows.push(Value::Array(hr));
    }
    for (i, row) in rows[data_start..].iter().enumerate() {
        let mut cells = row.as_array().cloned().unwrap_or_default();
        // The window covers data rows [i-window+1 ..= i].
        let cell = if i + 1 >= window {
            let slice = &nums[i + 1 - window..=i];
            if slice.iter().all(Option::is_some) {
                let avg = slice.iter().flatten().sum::<f64>() / window as f64;
                let v = match decimals {
                    Some(d) => {
                        let f = 10f64.powi(d as i32);
                        (avg * f).round() / f
                    }
                    None => avg,
                };
                json!(v)
            } else {
                json!("")
            }
        } else {
            json!("")
        };
        cells.push(cell);
        new_rows.push(Value::Array(cells));
    }
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "column": into }))
}

/// Append a row-over-row difference column (current − previous) for a numeric
/// column — useful for tracking change in a time series. opts: path, output,
/// column => name or 0-based index (required), into => new column header
/// (default "{column}_delta"), decimals => round, sheet, header (default true),
/// format. The first data row, or any row where the current/previous cell is
/// non-numeric, gets a blank. Returns `{ ok, path, column }`.
fn op_sheet_delta(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;
    let base = header_row
        .and_then(|hr| hr.get(col))
        .map(cell_to_string)
        .unwrap_or_else(|| format!("Col{}", col + 1));
    let into = opts
        .get("into")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{base}_delta"));
    let decimals = opts.get("decimals").and_then(Value::as_i64);

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let nums: Vec<Option<f64>> = rows[data_start..]
        .iter()
        .map(|r| {
            r.as_array()
                .and_then(|a| a.get(col))
                .and_then(sheet_cell_num)
        })
        .collect();

    let mut new_rows: Vec<Value> = Vec::with_capacity(rows.len());
    if data_start == 1 {
        let mut hr = rows[0].as_array().cloned().unwrap_or_default();
        hr.push(json!(into));
        new_rows.push(Value::Array(hr));
    }
    for (i, row) in rows[data_start..].iter().enumerate() {
        let mut cells = row.as_array().cloned().unwrap_or_default();
        let cell = match (i.checked_sub(1).and_then(|p| nums[p]), nums[i]) {
            (Some(prev), Some(cur)) => {
                let d = cur - prev;
                let v = match decimals {
                    Some(dp) => {
                        let f = 10f64.powi(dp as i32);
                        (d * f).round() / f
                    }
                    None => d,
                };
                json!(v)
            }
            _ => json!(""),
        };
        cells.push(cell);
        new_rows.push(Value::Array(cells));
    }
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "column": into }))
}

/// Clamp a numeric column's values to a range (winsorize / cap outliers). opts:
/// path, output, column => name or 0-based index (required), min and/or max =>
/// bounds (at least one required), into => write to a new column with this header
/// (default: clamp in place), sheet, header (default true), format. Non-numeric
/// cells pass through unchanged. Returns `{ ok, path, clamped }` (count of values
/// actually moved to a bound).
fn op_sheet_clamp(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let min = opts.get("min").and_then(Value::as_f64);
    let max = opts.get("max").and_then(Value::as_f64);
    if min.is_none() && max.is_none() {
        return Err(anyhow!("clamp needs at least one of min/max"));
    }
    let into = opts.get("into").and_then(Value::as_str).map(str::to_string);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;

    let clamp = |x: f64| -> f64 {
        let mut y = x;
        if let Some(lo) = min {
            y = y.max(lo);
        }
        if let Some(hi) = max {
            y = y.min(hi);
        }
        y
    };

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut clamped = 0u64;
    let new_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut cells = row.as_array().cloned().unwrap_or_default();
            if i < data_start {
                if let Some(name) = &into {
                    cells.push(json!(name));
                }
                return Value::Array(cells);
            }
            let src = cells.get(col).cloned().unwrap_or(Value::Null);
            let result = match sheet_cell_num(&src) {
                Some(x) => {
                    let y = clamp(x);
                    if y != x {
                        clamped += 1;
                    }
                    json!(y)
                }
                None => src.clone(),
            };
            match &into {
                Some(_) => cells.push(result),
                None => {
                    if col < cells.len() {
                        cells[col] = result;
                    } else {
                        cells.resize(col + 1, Value::Null);
                        cells[col] = result;
                    }
                }
            }
            Value::Array(cells)
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "clamped": clamped }))
}

/// Rename a column's header (distinct from `sheet_rename`, which renames a sheet
/// tab). opts: path, output, column => current name or 0-based index (required),
/// to => new header (required), sheet, format. Returns `{ ok, path, column }`
/// (the new header). Errors if the sheet has no header row.
fn op_sheet_rename_column(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let to = req_str(&opts, "to")?.to_string();

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let mut rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = rows.first().and_then(Value::as_array).map(|v| v.as_slice());
    let col = resolve_col(opts.get("column"), header_row)?;

    let hr = rows
        .first_mut()
        .and_then(|r| r.as_array_mut())
        .ok_or_else(|| anyhow!("sheet has no header row"))?;
    if col >= hr.len() {
        hr.resize(col + 1, Value::Null);
    }
    hr[col] = json!(to);
    sheets[target]["rows"] = Value::Array(rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "column": to }))
}

/// Explode a column of delimited values into multiple rows (SQL `unnest`; the
/// inverse of `group_concat`). Each data row is repeated once per split value,
/// with the other columns duplicated. opts: path, output, column => name or
/// 0-based index (required), sep => delimiter (default ","), trim => strip each
/// part (default true), sheet, header (default true), format. A blank cell yields
/// a single row with that cell blank. Returns `{ ok, path, rows }` (data rows
/// after exploding).
fn op_sheet_explode(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let sep = opts.get("sep").and_then(Value::as_str).unwrap_or(",");
    if sep.is_empty() {
        return Err(anyhow!("sep must be non-empty"));
    }
    let trim = opts.get("trim").and_then(flag_of).unwrap_or(true);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut new_rows: Vec<Value> = Vec::new();
    if data_start == 1 {
        new_rows.push(rows[0].clone());
    }
    let mut emitted = 0u64;
    for row in &rows[data_start..] {
        let cells = row.as_array().cloned().unwrap_or_default();
        let raw = cells.get(col).map(cell_to_string).unwrap_or_default();
        let parts: Vec<String> = if raw.is_empty() {
            vec![String::new()]
        } else {
            raw.split(sep)
                .map(|p| {
                    if trim {
                        p.trim().to_string()
                    } else {
                        p.to_string()
                    }
                })
                .collect()
        };
        for part in parts {
            let mut nc = cells.clone();
            if col < nc.len() {
                nc[col] = Value::String(part);
            } else {
                nc.resize(col + 1, Value::Null);
                nc[col] = Value::String(part);
            }
            new_rows.push(Value::Array(nc));
            emitted += 1;
        }
    }
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "rows": emitted }))
}

/// Recode a column's values via a lookup map (e.g. {"M":"Male","F":"Female"}).
/// opts: path, output, column => name or 0-based index (required), mapping =>
/// object of `{ old => new }` (required; keys matched against the cell as text),
/// default => value for unmapped cells (omit to leave them unchanged), into =>
/// write to a new column with this header (default: recode in place), sheet,
/// header (default true), format. Returns `{ ok, path, mapped }` (count of cells
/// changed).
fn op_sheet_map(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let mapping = opts
        .get("mapping")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("missing mapping (expected object of old => new)"))?
        .clone();
    let default = opts.get("default").cloned();
    let into = opts.get("into").and_then(Value::as_str).map(str::to_string);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut mapped = 0u64;
    let new_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut cells = row.as_array().cloned().unwrap_or_default();
            if i < data_start {
                if let Some(name) = &into {
                    cells.push(json!(name));
                }
                return Value::Array(cells);
            }
            let src = cells.get(col).cloned().unwrap_or(Value::Null);
            let key = cell_to_string(&src);
            let result = match mapping.get(&key) {
                Some(v) => {
                    mapped += 1;
                    v.clone()
                }
                None => match &default {
                    Some(d) => {
                        mapped += 1;
                        d.clone()
                    }
                    None => src.clone(),
                },
            };
            match &into {
                Some(_) => cells.push(result),
                None => {
                    if col < cells.len() {
                        cells[col] = result;
                    } else {
                        cells.resize(col + 1, Value::Null);
                        cells[col] = result;
                    }
                }
            }
            Value::Array(cells)
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "mapped": mapped }))
}

/// Partition a sheet into one file per distinct value of a column (e.g. split
/// sales by region). opts: path, column => name or 0-based index (required),
/// dir => output directory (required), prefix => filename prefix (default ""),
/// format => output extension (default: source's), header => repeat the header
/// row in each file (default true), sheet. Files are `{dir}/{prefix}{value}.{ext}`
/// with the value sanitized for the filename. Returns `{ count, files }`.
fn op_sheet_partition(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let dir = req_str(&opts, "dir")?;
    let prefix = opts.get("prefix").and_then(Value::as_str).unwrap_or("");
    let ext = opts
        .get("format")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| ext_of(path));
    let keep_header = opts.get("header").and_then(flag_of).unwrap_or(true);
    std::fs::create_dir_all(dir)?;

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header_row = if keep_header { rows.first() } else { None };
    let col = resolve_col(
        opts.get("column"),
        header_row.and_then(Value::as_array).map(|v| v.as_slice()),
    )?;

    // Group data rows by the column value, preserving first-seen order.
    let data_start = if keep_header && !rows.is_empty() {
        1
    } else {
        0
    };
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<Value>> =
        std::collections::HashMap::new();
    for row in &rows[data_start..] {
        let key = row
            .as_array()
            .and_then(|a| a.get(col))
            .map(cell_to_string)
            .unwrap_or_default();
        groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Vec::new()
        });
        groups.get_mut(&key).unwrap().push(row.clone());
    }

    let sanitize = |s: &str| -> String {
        let cleaned: String = s
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        if cleaned.is_empty() {
            "blank".to_string()
        } else {
            cleaned
        }
    };

    let mut files = Vec::new();
    for key in &order {
        let mut out_rows: Vec<Value> = Vec::new();
        if let Some(h) = header_row {
            out_rows.push(h.clone());
        }
        out_rows.extend(groups.remove(key).unwrap_or_default());
        let out = format!("{dir}/{prefix}{}.{ext}", sanitize(key));
        op_sheet_write(json!({
            "path": out,
            "sheets": [{ "name": "Sheet1", "rows": out_rows }],
            "format": ext,
        }))?;
        files.push(out);
    }
    Ok(json!({ "count": files.len(), "files": files }))
}

/// Sort data rows by multiple columns (the multi-key analogue of `sheet_sort`).
/// opts: path, output, keys => array of `{ column => name/index, descending? }`
/// in priority order (required), sheet, header (default true), format. Cells
/// compare numerically when both are numbers, else as text; `descending` flips a
/// key's order. Returns `{ ok, path, sorted }`.
fn op_sheet_multisort(opts: Value) -> Result<Value> {
    use std::cmp::Ordering;
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };

    let key_specs = opts
        .get("keys")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing keys (expected array of {{column, descending?}})"))?;
    if key_specs.is_empty() {
        return Err(anyhow!("keys must list at least one column"));
    }
    // (column index, descending) per key, in priority order.
    let keys: Vec<(usize, bool)> = key_specs
        .iter()
        .map(|k| {
            let col = resolve_col(k.get("column"), header_row)?;
            let desc = opt_flag(k, "descending").unwrap_or(false);
            Ok((col, desc))
        })
        .collect::<Result<_>>()?;

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut data: Vec<Value> = rows[data_start..].to_vec();
    data.sort_by(|a, b| {
        for &(col, desc) in &keys {
            let ca = a
                .as_array()
                .and_then(|x| x.get(col))
                .unwrap_or(&Value::Null);
            let cb = b
                .as_array()
                .and_then(|x| x.get(col))
                .unwrap_or(&Value::Null);
            let ord = match (sheet_cell_num(ca), sheet_cell_num(cb)) {
                (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
                _ => cell_to_string(ca).cmp(&cell_to_string(cb)),
            };
            let ord = if desc { ord.reverse() } else { ord };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });

    let sorted = data.len();
    let mut new_rows: Vec<Value> = Vec::with_capacity(rows.len());
    if data_start == 1 {
        new_rows.push(rows[0].clone());
    }
    new_rows.extend(data);
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "sorted": sorted }))
}

/// 0-based column index → spreadsheet letters (0→A, 25→Z, 26→AA).
fn col_letters(mut c: usize) -> String {
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + (c % 26) as u8) as char);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s
}

/// Find cells matching a query across a workbook. opts: path, query (required),
/// ignore_case (default false), whole (exact cell match vs substring; default
/// false), sheet (restrict to one sheet name). Returns `{ matches: [{ sheet,
/// row, col, ref, value }], count }` with 1-based row/col and an A1 `ref`.
fn op_sheet_find(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let query = req_str(&opts, "query")?;
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);
    let whole = opts.get("whole").and_then(flag_of).unwrap_or(false);
    let restrict = opts.get("sheet").and_then(Value::as_str);
    let needle = if ignore_case {
        query.to_lowercase()
    } else {
        query.to_string()
    };

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut matches = Vec::new();
    for s in &sheets {
        let sname = s["name"].as_str().unwrap_or("");
        if restrict.is_some_and(|r| r != sname) {
            continue;
        }
        let Some(rows) = s["rows"].as_array() else {
            continue;
        };
        for (ri, row) in rows.iter().enumerate() {
            let Some(cells) = row.as_array() else {
                continue;
            };
            for (ci, cell) in cells.iter().enumerate() {
                let val = cell_to_string(cell);
                if val.is_empty() {
                    continue;
                }
                let hay = if ignore_case {
                    val.to_lowercase()
                } else {
                    val.clone()
                };
                let hit = if whole {
                    hay == needle
                } else {
                    hay.contains(&needle)
                };
                if hit {
                    matches.push(json!({
                        "sheet": sname,
                        "row": ri + 1,
                        "col": ci + 1,
                        "ref": format!("{}{}", col_letters(ci), ri + 1),
                        "value": val,
                    }));
                }
            }
        }
    }
    Ok(json!({ "count": matches.len(), "matches": matches }))
}

/// Read a sheet as records: the first row is taken as field names and each
/// subsequent row becomes an object keyed by those names. opts: path,
/// sheet => name or 0-based index (default first). Returns `{ fields, records,
/// count }`; missing trailing cells come back as null.
fn op_sheet_records(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let fields: Vec<String> = rows
        .first()
        .and_then(Value::as_array)
        .map(|hr| hr.iter().map(cell_to_string).collect())
        .unwrap_or_default();

    let mut records = Vec::new();
    for row in rows.iter().skip(1) {
        let cells = row.as_array();
        let mut obj = serde_json::Map::new();
        for (i, f) in fields.iter().enumerate() {
            let v = cells.and_then(|c| c.get(i)).cloned().unwrap_or(Value::Null);
            obj.insert(f.clone(), v);
        }
        records.push(Value::Object(obj));
    }
    Ok(json!({ "fields": fields, "count": records.len(), "records": records }))
}

/// Write records (an array of objects) to a sheet: a header row of field names
/// followed by one row per record. opts: path, records (required),
/// fields => explicit column order (default: keys in first-seen order across
/// records), sheet_name (default "Sheet1"), format => override. Returns
/// `{ ok, path, rows, fields }`.
fn op_records_write(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?.to_string();
    let records = opts
        .get("records")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing records (expected array of objects)"))?;

    let fields: Vec<String> = match opts.get("fields").and_then(Value::as_array) {
        Some(f) => f.iter().map(cell_to_string).collect(),
        None => {
            let mut seen = std::collections::HashSet::new();
            let mut order = Vec::new();
            for rec in records {
                if let Some(o) = rec.as_object() {
                    for k in o.keys() {
                        if seen.insert(k.clone()) {
                            order.push(k.clone());
                        }
                    }
                }
            }
            order
        }
    };

    let mut rows: Vec<Value> = Vec::with_capacity(records.len() + 1);
    rows.push(json!(fields));
    for rec in records {
        let row: Vec<Value> = fields
            .iter()
            .map(|f| rec.get(f).cloned().unwrap_or(Value::Null))
            .collect();
        rows.push(Value::Array(row));
    }

    let sheet_name = opts
        .get("sheet_name")
        .and_then(Value::as_str)
        .unwrap_or("Sheet1");
    let mut wopts = json!({ "path": path, "sheets": [{ "name": sheet_name, "rows": rows }] });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": path, "rows": records.len(), "fields": fields }))
}

/// Export a sheet to a JSON file as an array of header-keyed objects. opts:
/// path, output (.json), sheet => name/index, pretty => bool (default true).
/// Returns `{ ok, path, count }`.
fn op_sheet_to_json(opts: Value) -> Result<Value> {
    let output = req_str(&opts, "output")?.to_string();
    let recs = op_sheet_records(opts.clone())?;
    let records = recs.get("records").cloned().unwrap_or_else(|| json!([]));
    let pretty = opts.get("pretty").and_then(flag_of).unwrap_or(true);
    let text = if pretty {
        serde_json::to_string_pretty(&records)?
    } else {
        serde_json::to_string(&records)?
    };
    std::fs::write(&output, text)?;
    Ok(
        json!({ "ok": true, "path": output, "count": recs.get("count").cloned().unwrap_or(json!(0)) }),
    )
}

/// Export a sheet as JSON Lines (NDJSON): one compact header-keyed JSON object
/// per data row, newline-separated — the standard format for data pipelines and
/// log ingestion. opts: path, output (.jsonl/.ndjson), sheet => name/index.
/// Returns `{ ok, path, count }`.
fn op_sheet_to_ndjson(opts: Value) -> Result<Value> {
    let output = req_str(&opts, "output")?.to_string();
    let recs = op_sheet_records(opts.clone())?;
    let records = recs
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut text = String::new();
    for rec in &records {
        text.push_str(&serde_json::to_string(rec)?);
        text.push('\n');
    }
    std::fs::write(&output, text)?;
    Ok(json!({ "ok": true, "path": output, "count": records.len() }))
}

/// Import JSON Lines (NDJSON) into a spreadsheet — the inverse of
/// `sheet_to_ndjson`. opts: input => a `.jsonl`/`.ndjson` file, or ndjson => the
/// text directly; output (required) => sheet path; fields => explicit column
/// order (default: keys in first-seen order); sheet_name (default "Sheet1");
/// format => override. Blank lines are skipped; each non-blank line must be a
/// JSON object. Returns `{ ok, path, rows, fields }`.
fn op_ndjson_to_sheet(opts: Value) -> Result<Value> {
    let text = match opts.get("ndjson").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => {
            let input = req_str(&opts, "input")?;
            String::from_utf8_lossy(&std::fs::read(input)?).into_owned()
        }
    };
    let mut records: Vec<Value> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line.trim())
            .map_err(|e| anyhow!("line {}: invalid JSON: {e}", i + 1))?;
        if !v.is_object() {
            return Err(anyhow!("line {}: expected a JSON object", i + 1));
        }
        records.push(v);
    }

    let mut wopts = json!({ "path": req_str(&opts, "output")?, "records": records });
    for k in ["fields", "sheet_name", "format"] {
        if let Some(val) = opts.get(k) {
            wopts[k] = val.clone();
        }
    }
    op_records_write(wopts)
}

/// Import a JSON file (array of objects) into a spreadsheet. opts: input (.json),
/// output (sheet path), fields => explicit column order, sheet_name, format.
/// Returns `{ ok, path, rows, fields }` (from records_write).
fn op_json_to_sheet(opts: Value) -> Result<Value> {
    let input = req_str(&opts, "input")?;
    let output = req_str(&opts, "output")?.to_string();
    let text = std::fs::read_to_string(input)?;
    let records: Value = serde_json::from_str(&text).map_err(|e| anyhow!("parse {input}: {e}"))?;
    if !records.is_array() {
        return Err(anyhow!("json must be an array of objects"));
    }
    let mut wopts = json!({ "path": output, "records": records });
    for k in ["fields", "sheet_name", "format"] {
        if let Some(v) = opts.get(k) {
            wopts[k] = v.clone();
        }
    }
    op_records_write(wopts)
}

/// Sort a sheet's data rows by a column, preserving the header. opts: path,
/// output, by => column name or 0-based index (required), sheet => name/index
/// (default first), header => bool (default true), descending => bool,
/// numeric => bool (default: auto-detect), ignore_case => bool (text mode),
/// format => override. Stable sort; all other sheets pass through unchanged.
/// Returns `{ ok, path, sorted, column }`.
fn op_sheet_sort(opts: Value) -> Result<Value> {
    use std::cmp::Ordering;
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let descending = opts.get("descending").and_then(flag_of).unwrap_or(false);
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);

    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array)
    } else {
        None
    };
    let col = match opts.get("by") {
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| anyhow!("bad column index"))? as usize,
        Some(Value::String(name)) => header_row
            .and_then(|hr| hr.iter().position(|c| cell_to_string(c) == *name))
            .ok_or_else(|| anyhow!("column not found: {name}"))?,
        _ => return Err(anyhow!("missing by (column name or 0-based index)")),
    };

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut data: Vec<Value> = rows[data_start..].to_vec();
    let cell_at = |row: &Value| -> Value {
        row.as_array()
            .and_then(|a| a.get(col))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let numeric = match opts.get("numeric").and_then(flag_of) {
        Some(b) => b,
        None => data
            .iter()
            .all(|r| sheet_cell_blank(&cell_at(r)) || sheet_cell_num(&cell_at(r)).is_some()),
    };

    data.sort_by(|a, b| {
        let (ca, cb) = (cell_at(a), cell_at(b));
        let ord = if numeric {
            let na = sheet_cell_num(&ca).unwrap_or(f64::INFINITY);
            let nb = sheet_cell_num(&cb).unwrap_or(f64::INFINITY);
            na.partial_cmp(&nb).unwrap_or(Ordering::Equal)
        } else {
            let (mut ka, mut kb) = (cell_to_string(&ca), cell_to_string(&cb));
            if ignore_case {
                ka = ka.to_lowercase();
                kb = kb.to_lowercase();
            }
            ka.cmp(&kb)
        };
        if descending {
            ord.reverse()
        } else {
            ord
        }
    });

    let sorted = data.len();
    let mut new_rows: Vec<Value> = Vec::with_capacity(rows.len());
    if data_start == 1 {
        new_rows.push(rows[0].clone());
    }
    new_rows.extend(data);
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "sorted": sorted, "column": col }))
}

/// Append a rank column to a sheet without reordering its rows (Excel `RANK` /
/// SQL `RANK()`). opts: path, output, by => column (name or 0-based index,
/// required), ascending => smallest value ranks first (default false = largest
/// first), dense => dense ranking 1,2,2,3 (default false = competition 1,2,2,4),
/// name => new column header (default "rank"), sheet, header, format. Rows whose
/// `by` cell is non-numeric get a blank rank. Returns `{ ok, path, ranked }`.
fn op_sheet_rank(opts: Value) -> Result<Value> {
    use std::cmp::Ordering;
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let ascending = opts.get("ascending").and_then(flag_of).unwrap_or(false);
    let dense = opts.get("dense").and_then(flag_of).unwrap_or(false);
    let name = opts.get("name").and_then(Value::as_str).unwrap_or("rank");

    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array)
    } else {
        None
    };
    let col = match opts.get("by") {
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| anyhow!("bad column index"))? as usize,
        Some(Value::String(name)) => header_row
            .and_then(|hr| hr.iter().position(|c| cell_to_string(c) == *name))
            .ok_or_else(|| anyhow!("column not found: {name}"))?,
        _ => return Err(anyhow!("missing by (column name or 0-based index)")),
    };

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let cell_num = |row: &Value| -> Option<f64> {
        row.as_array()
            .and_then(|a| a.get(col))
            .and_then(sheet_cell_num)
    };

    // Distinct numeric values sorted in ranking order; index into this list (with
    // ties skipped for competition ranking) gives each value's rank.
    let mut present: Vec<f64> = rows[data_start..].iter().filter_map(cell_num).collect();
    present.sort_by(|a, b| {
        let ord = a.partial_cmp(b).unwrap_or(Ordering::Equal);
        if ascending {
            ord
        } else {
            ord.reverse()
        }
    });
    let rank_of = |x: f64| -> i64 {
        if dense {
            // 1 + number of distinct values that outrank x.
            let mut better = 0i64;
            let mut last: Option<f64> = None;
            for &v in &present {
                if v == x {
                    break;
                }
                if last != Some(v) {
                    better += 1;
                    last = Some(v);
                }
            }
            better + 1
        } else {
            // 1 + number of values strictly outranking x (competition).
            present.iter().take_while(|&&v| v != x).count() as i64 + 1
        }
    };

    let mut ranked = 0i64;
    let new_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut cells = row.as_array().cloned().unwrap_or_default();
            if i < data_start {
                cells.push(json!(name));
            } else if let Some(x) = cell_num(row) {
                ranked += 1;
                cells.push(json!(rank_of(x)));
            } else {
                cells.push(json!(""));
            }
            Value::Array(cells)
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "ranked": ranked, "column": col }))
}

/// Whether a cell satisfies `op` against `value`. String ops: eq, ne, contains,
/// not_contains (honour `ignore_case`). Numeric ops: gt, lt, ge, le (both sides
/// must parse as numbers, else no match).
fn cell_matches(cell: &Value, op: &str, value: &Value, ignore_case: bool) -> bool {
    match op {
        "gt" | "lt" | "ge" | "le" => match (sheet_cell_num(cell), sheet_cell_num(value)) {
            (Some(x), Some(y)) => match op {
                "gt" => x > y,
                "lt" => x < y,
                "ge" => x >= y,
                _ => x <= y,
            },
            _ => false,
        },
        _ => {
            let (mut a, mut b) = (cell_to_string(cell), cell_to_string(value));
            if ignore_case {
                a = a.to_lowercase();
                b = b.to_lowercase();
            }
            match op {
                "eq" => a == b,
                "ne" => a != b,
                "contains" => a.contains(&b),
                "not_contains" => !a.contains(&b),
                _ => false,
            }
        }
    }
}

/// Keep only rows whose cell in a column satisfies a predicate, preserving the
/// header. opts: path, output, by => column name/index (required), op => one of
/// eq|ne|contains|not_contains|gt|lt|ge|le (default eq), value, sheet, header
/// (default true), ignore_case, format. Other sheets pass through unchanged.
/// Returns `{ ok, path, kept, removed, column }`.
fn op_sheet_filter(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let op = opts.get("op").and_then(Value::as_str).unwrap_or("eq");
    let value = opts.get("value").cloned().unwrap_or(Value::Null);
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array)
    } else {
        None
    };
    let col = match opts.get("by") {
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| anyhow!("bad column index"))? as usize,
        Some(Value::String(name)) => header_row
            .and_then(|hr| hr.iter().position(|c| cell_to_string(c) == *name))
            .ok_or_else(|| anyhow!("column not found: {name}"))?,
        _ => return Err(anyhow!("missing by (column name or 0-based index)")),
    };

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let total = rows.len() - data_start;
    let kept: Vec<Value> = rows[data_start..]
        .iter()
        .filter(|row| {
            let cell = row
                .as_array()
                .and_then(|a| a.get(col))
                .cloned()
                .unwrap_or(Value::Null);
            cell_matches(&cell, op, &value, ignore_case)
        })
        .cloned()
        .collect();

    let kept_n = kept.len();
    let mut new_rows: Vec<Value> = Vec::with_capacity(kept_n + 1);
    if data_start == 1 {
        new_rows.push(rows[0].clone());
    }
    new_rows.extend(kept);
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(
        json!({ "ok": true, "path": output, "kept": kept_n, "removed": total - kept_n, "column": col }),
    )
}

/// Resolve a column selector (0-based index or header name) to an index.
fn resolve_col(by: Option<&Value>, header_row: Option<&[Value]>) -> Result<usize> {
    match by {
        Some(Value::Number(n)) => {
            Ok(n.as_u64().ok_or_else(|| anyhow!("bad column index"))? as usize)
        }
        Some(Value::String(name)) => header_row
            .and_then(|hr| hr.iter().position(|c| cell_to_string(c) == *name))
            .ok_or_else(|| anyhow!("column not found: {name}")),
        _ => Err(anyhow!("column must be a name or 0-based index")),
    }
}

/// Group rows by a column and aggregate another (SQL GROUP BY). opts: path,
/// output, group_by => column name/index (required), value => column to
/// aggregate (required for sum/mean/min/max), agg => count|sum|mean|min|max
/// (default count), sheet, header (default true), format. Output is a two-column
/// sheet sorted by group key. Returns `{ ok, path, groups }`.
fn op_sheet_aggregate(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let agg = opts.get("agg").and_then(Value::as_str).unwrap_or("count");
    if !matches!(agg, "count" | "sum" | "mean" | "min" | "max") {
        return Err(anyhow!("unknown agg: {agg}"));
    }

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let gcol = resolve_col(opts.get("group_by"), header_row)?;
    let vcol = match opts.get("value") {
        Some(_) => Some(resolve_col(opts.get("value"), header_row)?),
        None if agg == "count" => None,
        None => return Err(anyhow!("agg '{agg}' requires a value column")),
    };

    let cell_at = |row: &Value, c: usize| -> Value {
        row.as_array()
            .and_then(|a| a.get(c))
            .cloned()
            .unwrap_or(Value::Null)
    };
    // (count, sum, min, max, numeric_count) per group, sorted by key.
    let mut groups: std::collections::BTreeMap<String, (u64, f64, f64, f64, u64)> =
        std::collections::BTreeMap::new();
    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    for row in &rows[data_start..] {
        let key = cell_to_string(&cell_at(row, gcol));
        let e = groups
            .entry(key)
            .or_insert((0, 0.0, f64::INFINITY, f64::NEG_INFINITY, 0));
        e.0 += 1;
        if let Some(vc) = vcol {
            if let Some(x) = sheet_cell_num(&cell_at(row, vc)) {
                e.1 += x;
                e.2 = e.2.min(x);
                e.3 = e.3.max(x);
                e.4 += 1;
            }
        }
    }

    let group_name = header_row
        .and_then(|hr| hr.get(gcol))
        .map(cell_to_string)
        .unwrap_or_else(|| format!("Col{}", gcol + 1));
    let value_name = vcol.map(|vc| {
        header_row
            .and_then(|hr| hr.get(vc))
            .map(cell_to_string)
            .unwrap_or_else(|| format!("Col{}", vc + 1))
    });
    let agg_label = if agg == "count" {
        "count".to_string()
    } else {
        format!("{agg}_{}", value_name.as_deref().unwrap_or("value"))
    };

    let group_count = groups.len();
    let mut out_rows: Vec<Value> = vec![json!([group_name, agg_label])];
    for (key, (count, sum, min, max, numc)) in groups {
        let v = match agg {
            "count" => json!(count),
            "sum" => json!(sum),
            "mean" if numc > 0 => json!(sum / numc as f64),
            "min" if numc > 0 => json!(min),
            "max" if numc > 0 => json!(max),
            _ => Value::Null,
        };
        out_rows.push(json!([key, v]));
    }

    let mut wopts =
        json!({ "path": output, "sheets": [{ "name": "Aggregate", "rows": out_rows }] });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "groups": group_count }))
}

/// Group rows by one column and concatenate another column's values per group
/// (SQL `GROUP_CONCAT`). opts: path, output, group_by => grouping column,
/// value => column whose values are joined (both name or 0-based index,
/// required), sep => separator (default ", "), distinct => drop duplicate values
/// within a group (default false), sheet, header (default true), format. Output
/// is a 2-column sheet `[group, "{value}_list"]` sorted by group key; blank
/// values are skipped. Returns `{ ok, path, groups }`.
fn op_sheet_group_concat(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let sep = opts.get("sep").and_then(Value::as_str).unwrap_or(", ");
    let distinct = opts.get("distinct").and_then(flag_of).unwrap_or(false);

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let gcol = resolve_col(opts.get("group_by"), header_row)?;
    let vcol = resolve_col(opts.get("value"), header_row)?;
    let gname = header_row
        .and_then(|hr| hr.get(gcol))
        .map(cell_to_string)
        .unwrap_or_else(|| format!("Col{}", gcol + 1));
    let vname = header_row
        .and_then(|hr| hr.get(vcol))
        .map(cell_to_string)
        .unwrap_or_else(|| format!("Col{}", vcol + 1));

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut groups: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for row in &rows[data_start..] {
        let cells = row.as_array();
        let key = cells
            .and_then(|a| a.get(gcol))
            .map(cell_to_string)
            .unwrap_or_default();
        let val = cells
            .and_then(|a| a.get(vcol))
            .map(cell_to_string)
            .unwrap_or_default();
        if val.trim().is_empty() {
            continue;
        }
        let e = groups.entry(key).or_default();
        if !distinct || !e.contains(&val) {
            e.push(val);
        }
    }

    let group_count = groups.len();
    let mut out_rows: Vec<Value> = vec![json!([gname, format!("{vname}_list")])];
    for (key, vals) in groups {
        out_rows.push(json!([key, vals.join(sep)]));
    }

    let mut wopts = json!({ "path": output, "sheets": [{ "name": "Grouped", "rows": out_rows }] });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "groups": group_count }))
}

/// VLOOKUP-style single-value lookup: find `key` in the `lookup` column and
/// return the corresponding cell from the `result` column. opts: path, lookup =>
/// search column, key => value to match, result => column to return (lookup and
/// result are name or 0-based index; all required), ignore_case => fold case when
/// matching strings (default false), sheet, header (default true). Returns
/// `{ found, value, row }` (row is the 0-based data-row index, -1 if not found;
/// value is null when not found).
fn op_sheet_lookup(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let key = opts
        .get("key")
        .map(cell_to_string)
        .ok_or_else(|| anyhow!("missing key"))?;
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let lcol = resolve_col(opts.get("lookup"), header_row)?;
    let rcol = resolve_col(opts.get("result"), header_row)?;

    let want = if ignore_case { key.to_lowercase() } else { key };
    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    for (i, row) in rows[data_start..].iter().enumerate() {
        let cell = row
            .as_array()
            .and_then(|a| a.get(lcol))
            .map(cell_to_string)
            .unwrap_or_default();
        let hay = if ignore_case {
            cell.to_lowercase()
        } else {
            cell
        };
        if hay == want {
            let value = row
                .as_array()
                .and_then(|a| a.get(rcol))
                .cloned()
                .unwrap_or(Value::Null);
            return Ok(json!({ "found": true, "value": value, "row": i }));
        }
    }
    Ok(json!({ "found": false, "value": Value::Null, "row": -1 }))
}

/// Count data rows whose `column` matches a predicate (Excel `COUNTIF`). opts:
/// path, column => name or 0-based index (required), op => eq|ne|contains|
/// not_contains|gt|lt|ge|le (default eq), value => comparison value, ignore_case
/// (default false), sheet, header (default true). Returns `{ count }`.
fn op_sheet_countif(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let op = opts.get("op").and_then(Value::as_str).unwrap_or("eq");
    let value = opts.get("value").cloned().unwrap_or(Value::Null);
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;
    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;
    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let count = rows[data_start..]
        .iter()
        .filter(|row| {
            let cell = row
                .as_array()
                .and_then(|a| a.get(col))
                .unwrap_or(&Value::Null);
            cell_matches(cell, op, &value, ignore_case)
        })
        .count();
    Ok(json!({ "count": count }))
}

/// Sum a numeric column over rows whose `column` matches a predicate (Excel
/// `SUMIF`). opts: path, column => the test column, op/value/ignore_case (as
/// `countif`), sum => the column to sum (name or 0-based index; default = the
/// test column), sheet, header (default true). Returns `{ sum, count }` (count
/// of matched rows). Non-numeric summed cells contribute 0.
fn op_sheet_sumif(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let op = opts.get("op").and_then(Value::as_str).unwrap_or("eq");
    let value = opts.get("value").cloned().unwrap_or(Value::Null);
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;
    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;
    let sum_col = match opts.get("sum") {
        Some(v) => resolve_col(Some(v), header_row)?,
        None => col,
    };
    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut sum = 0f64;
    let mut count = 0u64;
    for row in &rows[data_start..] {
        let cells = row.as_array();
        let cell = cells.and_then(|a| a.get(col)).unwrap_or(&Value::Null);
        if cell_matches(cell, op, &value, ignore_case) {
            count += 1;
            if let Some(x) = cells.and_then(|a| a.get(sum_col)).and_then(sheet_cell_num) {
                sum += x;
            }
        }
    }
    Ok(json!({ "sum": sum, "count": count }))
}

/// Frequency analysis (value-counts) of a single column, returned in memory and
/// sorted by count descending (pandas `value_counts`). Unlike `sheet_aggregate`
/// (which writes a file sorted by key), this is a pure read for analysis. opts:
/// path, column => name or 0-based index (required), sheet, header (default
/// true), ignore_case => fold case when grouping (default false), top => keep
/// only the N most frequent. Blank cells are skipped. Returns `{ column, total,
/// distinct, values: [{ value, count, pct }] }` (pct = percent of total, 0..100).
fn op_sheet_freq(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;
    let col_name = header_row
        .and_then(|hr| hr.get(col))
        .map(cell_to_string)
        .unwrap_or_else(|| format!("Col{}", col + 1));

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    // Preserve the first-seen display form while grouping on a (possibly folded)
    // key, and keep first-seen order so equal counts tie-break deterministically.
    let mut counts: std::collections::HashMap<String, (String, u64)> =
        std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut total = 0u64;
    for row in &rows[data_start..] {
        let cell = row
            .as_array()
            .and_then(|a| a.get(col))
            .unwrap_or(&Value::Null);
        if sheet_cell_blank(cell) {
            continue;
        }
        let display = cell_to_string(cell);
        let key = if ignore_case {
            display.to_lowercase()
        } else {
            display.clone()
        };
        let e = counts.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            (display, 0)
        });
        e.1 += 1;
        total += 1;
    }

    let mut entries: Vec<(String, u64)> = order
        .into_iter()
        .map(|k| counts.remove(&k).unwrap())
        .collect();
    // Count descending; ties keep first-seen order (stable sort).
    entries.sort_by_key(|e| std::cmp::Reverse(e.1));
    if let Some(top) = opts.get("top").and_then(Value::as_u64) {
        entries.truncate(top as usize);
    }

    let values: Vec<Value> = entries
        .iter()
        .map(|(display, count)| {
            let pct = if total > 0 {
                *count as f64 / total as f64 * 100.0
            } else {
                0.0
            };
            json!({ "value": display, "count": count, "pct": pct })
        })
        .collect();
    Ok(json!({
        "column": col_name,
        "total": total,
        "distinct": values.len(),
        "values": values,
    }))
}

/// Split one column into several by a delimiter (Excel "Text to Columns" /
/// pandas `str.split(expand=True)`). opts: path, output, column => name or
/// 0-based index (required), delimiter => separator (default ","), into =>
/// explicit new-column names (else `{name}_1..N`), max => cap the number of
/// output fields (remaining delimiters stay in the last field), trim => strip
/// from each part (default true), keep => keep the original column before the new
/// ones (default false = replace), sheet, header, format. The new-column count is
/// `into.len()` when given, else the widest split across data rows; short rows are
/// padded with blanks. Returns `{ ok, path, columns }`.
fn op_sheet_split_column(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let delimiter = opts.get("delimiter").and_then(Value::as_str).unwrap_or(",");
    if delimiter.is_empty() {
        return Err(anyhow!("delimiter must be non-empty"));
    }
    let trim = opts.get("trim").and_then(flag_of).unwrap_or(true);
    let keep = opts.get("keep").and_then(flag_of).unwrap_or(false);
    let max = opts.get("max").and_then(Value::as_u64).map(|n| n as usize);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;
    let base_name = header_row
        .and_then(|hr| hr.get(col))
        .map(cell_to_string)
        .unwrap_or_else(|| format!("Col{}", col + 1));

    let split_cell = |s: &str| -> Vec<String> {
        let parts: Vec<&str> = match max {
            Some(m) if m >= 1 => s.splitn(m, delimiter).collect(),
            _ => s.split(delimiter).collect(),
        };
        parts
            .into_iter()
            .map(|p| {
                if trim {
                    p.trim().to_string()
                } else {
                    p.to_string()
                }
            })
            .collect()
    };

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let into_names: Option<Vec<String>> = opts
        .get("into")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(cell_to_string).collect());
    let ncols_new = match &into_names {
        Some(names) => names.len(),
        None => rows[data_start..]
            .iter()
            .map(|r| {
                let cell = r
                    .as_array()
                    .and_then(|a| a.get(col))
                    .unwrap_or(&Value::Null);
                split_cell(&cell_to_string(cell)).len()
            })
            .max()
            .unwrap_or(1),
    };
    let new_headers: Vec<String> = match into_names {
        Some(names) => names,
        None => (1..=ncols_new)
            .map(|i| format!("{base_name}_{i}"))
            .collect(),
    };

    let new_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let cells = row.as_array().cloned().unwrap_or_default();
            let mut out: Vec<Value> = cells.iter().take(col).cloned().collect();
            if i < data_start {
                if keep {
                    out.push(cells.get(col).cloned().unwrap_or(Value::Null));
                }
                out.extend(new_headers.iter().map(|n| json!(n)));
            } else {
                if keep {
                    out.push(cells.get(col).cloned().unwrap_or(Value::Null));
                }
                let raw = cells.get(col).map(cell_to_string).unwrap_or_default();
                let mut parts = split_cell(&raw);
                parts.resize(ncols_new, String::new());
                out.extend(parts.into_iter().map(Value::String));
            }
            out.extend(cells.iter().skip(col + 1).cloned());
            Value::Array(out)
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "columns": ncols_new }))
}

/// Join several columns into one with a separator (Excel `TEXTJOIN`/`CONCAT`);
/// the inverse of `sheet_split_column`. opts: path, output, columns => list of
/// names or 0-based indices to join, in join order (required), separator =>
/// default " ", into => new-column header (default "merged"), skip_blanks => drop
/// blank cells before joining (default false), keep => keep the source columns and
/// append the merged column at the end (default false = replace them with the
/// merged column at the leftmost source position), sheet, header, format. Returns
/// `{ ok, path, into }`.
fn op_sheet_concat_columns(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let separator = opts.get("separator").and_then(Value::as_str).unwrap_or(" ");
    let into = opts.get("into").and_then(Value::as_str).unwrap_or("merged");
    let skip_blanks = opts.get("skip_blanks").and_then(flag_of).unwrap_or(false);
    let keep = opts.get("keep").and_then(flag_of).unwrap_or(false);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };

    let col_list = opts
        .get("columns")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing columns (expected array of names or indices)"))?;
    if col_list.is_empty() {
        return Err(anyhow!("columns must list at least one column"));
    }
    // Resolve in user-specified join order.
    let cols: Vec<usize> = col_list
        .iter()
        .map(|v| resolve_col(Some(v), header_row))
        .collect::<Result<_>>()?;
    let merge_set: std::collections::HashSet<usize> = cols.iter().copied().collect();
    let leftmost = *cols.iter().min().unwrap();

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let new_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let cells = row.as_array().cloned().unwrap_or_default();
            let merged = if i < data_start {
                json!(into)
            } else {
                let mut parts: Vec<String> = cols
                    .iter()
                    .map(|&c| cells.get(c).map(cell_to_string).unwrap_or_default())
                    .collect();
                if skip_blanks {
                    parts.retain(|p| !p.trim().is_empty());
                }
                json!(parts.join(separator))
            };
            if keep {
                let mut out = cells.clone();
                out.push(merged);
                return Value::Array(out);
            }
            // Replace: emit merged at the leftmost source position, drop the rest.
            let out: Vec<Value> = cells
                .iter()
                .enumerate()
                .filter_map(|(j, cell)| {
                    if j == leftmost {
                        Some(merged.clone())
                    } else if merge_set.contains(&j) {
                        None
                    } else {
                        Some(cell.clone())
                    }
                })
                .collect();
            Value::Array(out)
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "into": into }))
}

/// Build a pivot table: group rows by one column and columns by another, with a
/// value aggregated into each cell (Excel PivotTable). opts: path, output,
/// rows => row-group column, cols => column-group column, value => aggregated
/// column (required for sum/mean/min/max), agg => count|sum|mean|min|max
/// (default sum), sheet, header (default true), format. Output is a matrix sheet
/// sorted by row and column keys; missing combos are 0 (count/sum) or blank.
/// Returns `{ ok, path, rows, cols }`.
fn op_sheet_pivot(opts: Value) -> Result<Value> {
    use std::collections::{BTreeSet, HashMap};
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let agg = opts.get("agg").and_then(Value::as_str).unwrap_or("sum");
    if !matches!(agg, "count" | "sum" | "mean" | "min" | "max") {
        return Err(anyhow!("unknown agg: {agg}"));
    }

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let rcol = resolve_col(opts.get("rows"), header_row)?;
    let ccol = resolve_col(opts.get("cols"), header_row)?;
    let vcol = match opts.get("value") {
        Some(_) => Some(resolve_col(opts.get("value"), header_row)?),
        None if agg == "count" => None,
        None => return Err(anyhow!("agg '{agg}' requires a value column")),
    };

    let cell_at = |row: &Value, c: usize| -> Value {
        row.as_array()
            .and_then(|a| a.get(c))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mut row_keys: BTreeSet<String> = BTreeSet::new();
    let mut col_keys: BTreeSet<String> = BTreeSet::new();
    let mut acc: HashMap<(String, String), (u64, f64, f64, f64, u64)> = HashMap::new();
    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    for row in &rows[data_start..] {
        let rk = cell_to_string(&cell_at(row, rcol));
        let ck = cell_to_string(&cell_at(row, ccol));
        row_keys.insert(rk.clone());
        col_keys.insert(ck.clone());
        let e = acc
            .entry((rk, ck))
            .or_insert((0, 0.0, f64::INFINITY, f64::NEG_INFINITY, 0));
        e.0 += 1;
        if let Some(vc) = vcol {
            if let Some(x) = sheet_cell_num(&cell_at(row, vc)) {
                e.1 += x;
                e.2 = e.2.min(x);
                e.3 = e.3.max(x);
                e.4 += 1;
            }
        }
    }

    let row_name = header_row
        .and_then(|hr| hr.get(rcol))
        .map(cell_to_string)
        .unwrap_or_else(|| format!("Col{}", rcol + 1));
    let cols: Vec<String> = col_keys.into_iter().collect();
    let mut out_rows: Vec<Value> = Vec::with_capacity(row_keys.len() + 1);
    let mut head = vec![json!(row_name)];
    head.extend(cols.iter().map(|c| json!(c)));
    out_rows.push(Value::Array(head));

    let (n_rows, n_cols) = (row_keys.len(), cols.len());
    for rk in row_keys {
        let mut out_row = vec![json!(rk)];
        for ck in &cols {
            let cell = match acc.get(&(rk.clone(), ck.clone())) {
                Some(&(count, sum, min, max, numc)) => match agg {
                    "count" => json!(count),
                    "sum" => json!(sum),
                    "mean" if numc > 0 => json!(sum / numc as f64),
                    "min" if numc > 0 => json!(min),
                    "max" if numc > 0 => json!(max),
                    _ => json!(""),
                },
                None => {
                    if agg == "count" || agg == "sum" {
                        json!(0)
                    } else {
                        json!("")
                    }
                }
            };
            out_row.push(cell);
        }
        out_rows.push(Value::Array(out_row));
    }

    let mut wopts = json!({ "path": output, "sheets": [{ "name": "Pivot", "rows": out_rows }] });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "rows": n_rows, "cols": n_cols }))
}

/// Unpivot (melt) a sheet from wide to long. opts: path, output,
/// id_vars => column(s) to keep as identifiers (name/index or array),
/// value_vars => columns to unpivot (default: all non-id columns),
/// var_name (default "variable"), value_name (default "value"), sheet, header
/// (default true), format. Each value column becomes one row per data row.
/// Returns `{ ok, path, rows }`.
fn op_sheet_unpivot(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let var_name = opts
        .get("var_name")
        .and_then(Value::as_str)
        .unwrap_or("variable");
    let value_name = opts
        .get("value_name")
        .and_then(Value::as_str)
        .unwrap_or("value");

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let hr = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let ncols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);
    let names: Vec<String> = (0..ncols)
        .map(|c| {
            hr.and_then(|h| h.get(c))
                .map(cell_to_string)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("Col{}", c + 1))
        })
        .collect();

    let resolve_list = |v: Option<&Value>| -> Result<Vec<usize>> {
        match v {
            Some(Value::Array(a)) => a.iter().map(|c| resolve_col(Some(c), hr)).collect(),
            Some(x) if !x.is_null() => Ok(vec![resolve_col(Some(x), hr)?]),
            _ => Ok(vec![]),
        }
    };
    let id_cols = resolve_list(opts.get("id_vars"))?;
    let value_cols = match opts.get("value_vars") {
        Some(_) => resolve_list(opts.get("value_vars"))?,
        None => (0..ncols).filter(|c| !id_cols.contains(c)).collect(),
    };

    let cell = |row: &Value, c: usize| -> Value {
        row.as_array()
            .and_then(|a| a.get(c))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mut head: Vec<Value> = id_cols.iter().map(|&c| json!(names[c])).collect();
    head.push(json!(var_name));
    head.push(json!(value_name));
    let mut out_rows = vec![Value::Array(head)];

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    for row in &rows[data_start..] {
        let id_vals: Vec<Value> = id_cols.iter().map(|&c| cell(row, c)).collect();
        for &vc in &value_cols {
            let mut r = id_vals.clone();
            r.push(json!(names[vc]));
            r.push(cell(row, vc));
            out_rows.push(Value::Array(r));
        }
    }

    let n = out_rows.len() - 1;
    let mut wopts = json!({ "path": output, "sheets": [{ "name": "Melted", "rows": out_rows }] });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "rows": n }))
}

/// Join two spreadsheets on a key column (SQL JOIN). opts: left, right (paths),
/// output, on => shared key field name (or left_on + right_on), how =>
/// "inner" (default) | "left", left_sheet / right_sheet => name/index, format.
/// Output columns are the left fields then the right fields (the right key is
/// dropped; colliding right names get a `_right` suffix). Returns
/// `{ ok, path, rows, matched }`.
fn op_sheet_join(opts: Value) -> Result<Value> {
    use std::collections::{HashMap, HashSet};
    let left = req_str(&opts, "left")?;
    let right = req_str(&opts, "right")?;
    let output = req_str(&opts, "output")?.to_string();
    let how = opts.get("how").and_then(Value::as_str).unwrap_or("inner");
    let on = opts.get("on").and_then(Value::as_str);
    let left_on = opts
        .get("left_on")
        .and_then(Value::as_str)
        .or(on)
        .ok_or_else(|| anyhow!("missing on (or left_on/right_on)"))?;
    let right_on = opts
        .get("right_on")
        .and_then(Value::as_str)
        .or(on)
        .unwrap_or(left_on);

    let lr = op_sheet_records(json!({ "path": left, "sheet": opts.get("left_sheet") }))?;
    let rr = op_sheet_records(json!({ "path": right, "sheet": opts.get("right_sheet") }))?;
    let str_vec = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let lfields = str_vec(&lr["fields"]);
    let rfields = str_vec(&rr["fields"]);
    let empty: Vec<Value> = Vec::new();
    let lrecs = lr["records"].as_array().unwrap_or(&empty);
    let rrecs = rr["records"].as_array().unwrap_or(&empty);

    // Index right records by key value.
    let mut idx: HashMap<String, Vec<&Value>> = HashMap::new();
    for r in rrecs {
        let k = cell_to_string(r.get(right_on).unwrap_or(&Value::Null));
        idx.entry(k).or_default().push(r);
    }
    // Right output columns (drop the key; rename collisions).
    let lset: HashSet<&str> = lfields.iter().map(String::as_str).collect();
    let right_extra: Vec<(String, String)> = rfields
        .iter()
        .filter(|f| f.as_str() != right_on)
        .map(|f| {
            let out = if lset.contains(f.as_str()) {
                format!("{f}_right")
            } else {
                f.clone()
            };
            (out, f.clone())
        })
        .collect();

    let mut header: Vec<Value> = lfields.iter().map(|f| json!(f)).collect();
    header.extend(right_extra.iter().map(|(o, _)| json!(o)));
    let mut out_rows = vec![Value::Array(header)];
    let mut matched = 0u64;
    let left_vals = |lrec: &Value| -> Vec<Value> {
        lfields
            .iter()
            .map(|f| lrec.get(f).cloned().unwrap_or(Value::Null))
            .collect()
    };
    for lrec in lrecs {
        let k = cell_to_string(lrec.get(left_on).unwrap_or(&Value::Null));
        match idx.get(&k) {
            Some(rs) => {
                for r in rs {
                    matched += 1;
                    let mut row = left_vals(lrec);
                    row.extend(
                        right_extra
                            .iter()
                            .map(|(_, src)| r.get(src).cloned().unwrap_or(Value::Null)),
                    );
                    out_rows.push(Value::Array(row));
                }
            }
            None if how == "left" => {
                let mut row = left_vals(lrec);
                row.extend(right_extra.iter().map(|_| Value::Null));
                out_rows.push(Value::Array(row));
            }
            None => {}
        }
    }

    let n = out_rows.len() - 1;
    let mut wopts = json!({ "path": output, "sheets": [{ "name": "Join", "rows": out_rows }] });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "rows": n, "matched": matched }))
}

/// Project/reorder a sheet's columns. opts: path, output, columns => array of
/// column names or 0-based indices (required, in output order), sheet,
/// header => bool (names need true; default true), format. Every row (header
/// included) is reduced to the chosen columns. Other sheets pass through.
/// Returns `{ ok, path, columns }`.
fn op_sheet_select(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let wanted = opts
        .get("columns")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing columns (expected array of names or indices)"))?;

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let hr = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let cols: Vec<usize> = wanted
        .iter()
        .map(|c| resolve_col(Some(c), hr))
        .collect::<Result<_>>()?;

    let new_rows: Vec<Value> = rows
        .iter()
        .map(|row| {
            let arr = row.as_array();
            Value::Array(
                cols.iter()
                    .map(|&c| arr.and_then(|a| a.get(c)).cloned().unwrap_or(Value::Null))
                    .collect(),
            )
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "columns": cols.len() }))
}

/// Drop columns from a sheet (complement of `sheet_select`). opts: path, output,
/// columns => column name(s)/index(es) to remove, sheet, header (default true),
/// format. Returns `{ ok, path, columns }` (kept column count).
fn op_sheet_drop(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = select_sheet_rows(path, opts.get("sheet"))?;
    let hr = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let ncols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);
    let drop: Vec<usize> = match opts.get("columns") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|c| resolve_col(Some(c), hr))
            .collect::<Result<_>>()?,
        Some(v) if !v.is_null() => vec![resolve_col(Some(v), hr)?],
        _ => return Err(anyhow!("missing columns to drop")),
    };
    let keep: Vec<Value> = (0..ncols)
        .filter(|c| !drop.contains(c))
        .map(|c| json!(c))
        .collect();

    // Delegate to sheet_select with the kept column indices.
    let mut sopts = opts.clone();
    sopts["columns"] = Value::Array(keep);
    op_sheet_select(sopts)
}

/// Add a derived column to a sheet. opts: path, output, name => new header
/// (header row only), then either `value` => a constant for every data row, or
/// `concat` => [columns] joined by `sep` (default " "). sheet, header (default
/// true), format. Returns `{ ok, path, column }`.
fn op_sheet_add_column(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let name = req_str(&opts, "name")?.to_string();
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(n)) => sheets.iter().position(|s| s["name"] == *n),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let hr = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let concat: Option<Vec<usize>> = match opts.get("concat").and_then(Value::as_array) {
        Some(a) => Some(
            a.iter()
                .map(|c| resolve_col(Some(c), hr))
                .collect::<Result<_>>()?,
        ),
        None => None,
    };
    let sep = opts.get("sep").and_then(Value::as_str).unwrap_or(" ");
    let constant = opts.get("value").cloned().unwrap_or(Value::Null);
    let data_start = if header && !rows.is_empty() { 1 } else { 0 };

    let mut new_rows = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let mut arr = row.as_array().cloned().unwrap_or_default();
        if i < data_start {
            arr.push(json!(name));
        } else if let Some(cols) = &concat {
            let parts: Vec<String> = cols
                .iter()
                .map(|&c| cell_to_string(arr.get(c).unwrap_or(&Value::Null)))
                .collect();
            arr.push(json!(parts.join(sep)));
        } else {
            arr.push(constant.clone());
        }
        new_rows.push(Value::Array(arr));
    }
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "column": name }))
}

/// Append a totals row summing each numeric column. opts: path, output,
/// label => text for the first cell (default "Total"), sheet, header (skips the
/// header row from sums; default true), format. Non-numeric columns get a blank
/// total. Returns `{ ok, path, totals }` (number of summed columns).
fn op_sheet_totals(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let label = opts.get("label").and_then(Value::as_str).unwrap_or("Total");
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(n)) => sheets.iter().position(|s| s["name"] == *n),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let mut rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let ncols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);
    let data_start = if header && !rows.is_empty() { 1 } else { 0 };

    let mut sums = vec![0f64; ncols];
    let mut has_num = vec![false; ncols];
    for row in &rows[data_start..] {
        if let Some(a) = row.as_array() {
            for c in 0..ncols {
                if let Some(x) = a.get(c).and_then(sheet_cell_num) {
                    sums[c] += x;
                    has_num[c] = true;
                }
            }
        }
    }
    let total_row: Vec<Value> = (0..ncols)
        .map(|c| {
            if c == 0 {
                json!(label)
            } else if has_num[c] {
                json!(sums[c])
            } else {
                json!("")
            }
        })
        .collect();
    let summed = has_num.iter().filter(|&&b| b).count();
    rows.push(Value::Array(total_row));
    sheets[target]["rows"] = Value::Array(rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "totals": summed }))
}

/// ASCII case-insensitive substring replace (byte-length preserving, so byte
/// offsets stay valid). Returns (new string, count).
fn ascii_ci_replace(hay: &str, find: &str, rep: &str) -> (String, usize) {
    let (lh, lf) = (hay.to_ascii_lowercase(), find.to_ascii_lowercase());
    let mut out = String::new();
    let (mut i, mut n) = (0usize, 0usize);
    while let Some(pos) = lh[i..].find(&lf) {
        let start = i + pos;
        out.push_str(&hay[i..start]);
        out.push_str(rep);
        i = start + find.len();
        n += 1;
    }
    out.push_str(&hay[i..]);
    (out, n)
}

/// Find/replace text in a sheet's cells (works on any spreadsheet format,
/// including csv). opts: path, output (default in place), find (required),
/// replace (default ""), ignore_case, whole (replace whole cell vs substring),
/// column => restrict to one column, sheet, header (default true), format.
/// Only string cells are touched. Returns `{ ok, path, replaced }`.
fn op_sheet_replace(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let find = req_str(&opts, "find")?.to_string();
    if find.is_empty() {
        return Err(anyhow!("empty find"));
    }
    let replace = opts.get("replace").and_then(Value::as_str).unwrap_or("");
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);
    let whole = opts.get("whole").and_then(flag_of).unwrap_or(false);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let mut rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let hr = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col_restrict = match opts.get("column") {
        Some(v) if !v.is_null() => Some(resolve_col(Some(v), hr)?),
        _ => None,
    };

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut replaced = 0usize;
    for row in rows[data_start..].iter_mut() {
        let Some(arr) = row.as_array_mut() else {
            continue;
        };
        for (ci, cell) in arr.iter_mut().enumerate() {
            if col_restrict.is_some_and(|c| c != ci) {
                continue;
            }
            if let Value::String(s) = cell {
                if whole {
                    let hit = if ignore_case {
                        s.eq_ignore_ascii_case(&find)
                    } else {
                        *s == find
                    };
                    if hit {
                        *s = replace.to_string();
                        replaced += 1;
                    }
                } else if ignore_case {
                    let (new, n) = ascii_ci_replace(s, &find, replace);
                    if n > 0 {
                        *s = new;
                        replaced += n;
                    }
                } else {
                    let n = s.matches(&find).count();
                    if n > 0 {
                        *s = s.replace(&find, replace);
                        replaced += n;
                    }
                }
            }
        }
    }
    sheets[target]["rows"] = Value::Array(rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "replaced": replaced }))
}

/// Transpose a sheet — rows become columns and vice versa. opts: path, output,
/// sheet => name/index (default first), format. Other sheets pass through.
/// Returns `{ ok, path, rows, columns }` (dimensions of the transposed sheet).
fn op_sheet_transpose(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let ncols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);
    let mut new_rows: Vec<Value> = Vec::with_capacity(ncols);
    for c in 0..ncols {
        let col: Vec<Value> = rows
            .iter()
            .map(|r| {
                r.as_array()
                    .and_then(|a| a.get(c))
                    .cloned()
                    .unwrap_or(Value::Null)
            })
            .collect();
        new_rows.push(Value::Array(col));
    }
    let dims = (new_rows.len(), rows.len());
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "rows": dims.0, "columns": dims.1 }))
}

/// Remove duplicate data rows (SQL DISTINCT). opts: path, output,
/// by => a column name/index or an array of them (default: the whole row),
/// keep => "first" (default) | "last", sheet, header (default true), format.
/// Order is preserved; the header is kept. Returns `{ ok, path, kept, removed }`.
fn op_sheet_dedupe(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let keep_last = opts.get("keep").and_then(Value::as_str) == Some("last");

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let hr = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let cols: Option<Vec<usize>> = match opts.get("by") {
        None | Some(Value::Null) => None,
        Some(Value::Array(arr)) => Some(
            arr.iter()
                .map(|c| resolve_col(Some(c), hr))
                .collect::<Result<_>>()?,
        ),
        Some(v) => Some(vec![resolve_col(Some(v), hr)?]),
    };
    let row_key = |row: &Value| -> String {
        match &cols {
            Some(cs) => cs
                .iter()
                .map(|&c| {
                    cell_to_string(
                        row.as_array()
                            .and_then(|a| a.get(c))
                            .unwrap_or(&Value::Null),
                    )
                })
                .collect::<Vec<_>>()
                .join("\u{1}"),
            None => row.to_string(),
        }
    };

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let total = rows.len() - data_start;
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<Value> = if keep_last {
        let mut rev: Vec<Value> = Vec::new();
        for row in rows[data_start..].iter().rev() {
            if seen.insert(row_key(row)) {
                rev.push(row.clone());
            }
        }
        rev.reverse();
        rev
    } else {
        let mut out = Vec::new();
        for row in &rows[data_start..] {
            if seen.insert(row_key(row)) {
                out.push(row.clone());
            }
        }
        out
    };

    let kept_n = kept.len();
    let mut new_rows: Vec<Value> = Vec::with_capacity(kept_n + 1);
    if data_start == 1 {
        new_rows.push(rows[0].clone());
    }
    new_rows.append(&mut kept);
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "kept": kept_n, "removed": total - kept_n }))
}

/// Append rows to an existing sheet. opts: path, output (default: in place),
/// sheet => name/index (default first), and either `rows` => [[…], …] (raw rows)
/// or `records` => [{…}] (mapped to the sheet's existing header field order),
/// format. Returns `{ ok, path, added, rows }` (rows = new total).
fn op_sheet_append(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let mut rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let to_add: Vec<Value> = if let Some(raw) = opts.get("rows").and_then(Value::as_array) {
        raw.clone()
    } else if let Some(recs) = opts.get("records").and_then(Value::as_array) {
        let fields: Vec<String> = rows
            .first()
            .and_then(Value::as_array)
            .map(|h| h.iter().map(cell_to_string).collect())
            .unwrap_or_default();
        recs.iter()
            .map(|rec| {
                Value::Array(
                    fields
                        .iter()
                        .map(|f| rec.get(f).cloned().unwrap_or(Value::Null))
                        .collect(),
                )
            })
            .collect()
    } else {
        return Err(anyhow!("need rows or records to append"));
    };

    let added = to_add.len();
    rows.extend(to_add);
    let total = rows.len();
    sheets[target]["rows"] = Value::Array(rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "added": added, "rows": total }))
}

/// Fill blank cells in a sheet. opts: path, output (default: in place),
/// method => "ffill" (default; carry the last non-blank value down) | "value"
/// (use a constant `value`), by => column name/index or array (default: all
/// columns), value (for the constant method), sheet, header (default true),
/// format. Returns `{ ok, path, filled }`.
fn op_sheet_fill(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let method = opts
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("ffill");
    let fill_value = opts.get("value").cloned().unwrap_or(Value::Null);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let mut rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let hr = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let ncols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);
    let target_cols: Vec<usize> = match opts.get("by") {
        None | Some(Value::Null) => (0..ncols).collect(),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|c| resolve_col(Some(c), hr))
            .collect::<Result<_>>()?,
        Some(v) => vec![resolve_col(Some(v), hr)?],
    };

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    // Pad data rows to full width so trimmed trailing blanks are fillable.
    for row in rows[data_start..].iter_mut() {
        if let Some(arr) = row.as_array_mut() {
            while arr.len() < ncols {
                arr.push(Value::Null);
            }
        }
    }

    let mut last: std::collections::HashMap<usize, Value> = std::collections::HashMap::new();
    let mut filled = 0u64;
    for ri in data_start..rows.len() {
        let Some(arr) = rows[ri].as_array_mut() else {
            continue;
        };
        for &c in &target_cols {
            if c >= arr.len() {
                continue;
            }
            let blank = sheet_cell_blank(&arr[c]);
            if method == "value" {
                if blank {
                    arr[c] = fill_value.clone();
                    filled += 1;
                }
            } else if blank {
                if let Some(v) = last.get(&c) {
                    arr[c] = v.clone();
                    filled += 1;
                }
            } else {
                last.insert(c, arr[c].clone());
            }
        }
    }
    sheets[target]["rows"] = Value::Array(rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "filled": filled }))
}

/// Drop fully-empty rows and/or columns (clean up sparse/scraped data). opts:
/// path, output (default in place), rows => drop all-blank rows (default true),
/// cols => drop columns whose data cells are all blank (default false; the
/// header cell is dropped too), sheet, header (default true), format. The header
/// row is always kept. Returns `{ ok, path, rows_removed, cols_removed }`.
fn op_sheet_drop_empty(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let drop_rows = opts.get("rows").and_then(flag_of).unwrap_or(true);
    let drop_cols = opts.get("cols").and_then(flag_of).unwrap_or(false);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let ncols = rows
        .iter()
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .max()
        .unwrap_or(0);
    let data_start = if header && !rows.is_empty() { 1 } else { 0 };

    let cell_at = |row: &Value, c: usize| -> Value {
        row.as_array()
            .and_then(|a| a.get(c))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let row_blank = |row: &Value| (0..ncols).all(|c| sheet_cell_blank(&cell_at(row, c)));

    // A column is empty when every data-row cell is blank.
    let keep_col: Vec<bool> = (0..ncols)
        .map(|c| {
            !drop_cols
                || rows[data_start..].is_empty()
                || rows[data_start..]
                    .iter()
                    .any(|r| !sheet_cell_blank(&cell_at(r, c)))
        })
        .collect();
    let cols_removed = keep_col.iter().filter(|&&k| !k).count();

    let mut rows_removed = 0u64;
    let new_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .filter(|(i, row)| {
            if drop_rows && *i >= data_start && row_blank(row) {
                rows_removed += 1;
                false
            } else {
                true
            }
        })
        .map(|(_, row)| {
            if cols_removed == 0 {
                return row.clone();
            }
            let cells = row.as_array().cloned().unwrap_or_default();
            let kept: Vec<Value> = (0..ncols)
                .filter(|&c| keep_col[c])
                .map(|c| cells.get(c).cloned().unwrap_or(Value::Null))
                .collect();
            Value::Array(kept)
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(
        json!({ "ok": true, "path": output, "rows_removed": rows_removed, "cols_removed": cols_removed }),
    )
}

/// Prepend a header row of column names (for headerless data, e.g. a bare CSV).
/// opts: path, output, names => array of column names (required), sheet, format.
/// All existing rows become data rows. Returns `{ ok, path, columns }`.
fn op_sheet_add_header(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let names = opts
        .get("names")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing names (expected array of column names)"))?
        .clone();

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let mut rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    rows.insert(0, Value::Array(names.clone()));
    sheets[target]["rows"] = Value::Array(rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "columns": names.len() }))
}

/// Append a computed column from an arithmetic op between two columns, or a
/// column and a constant (e.g. `total = qty * price`). opts: path, output, into
/// => new column header (required), left => column name/index (required), op =>
/// one of `+ - * / %` (required), and either right => a second column or value =>
/// a numeric constant (one required), decimals => round, sheet, header (default
/// true), format. Rows where an operand is non-numeric (or division by zero) get
/// a blank. Returns `{ ok, path, column }`.
fn op_sheet_calc(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let into = req_str(&opts, "into")?.to_string();
    let op = req_str(&opts, "op")?.to_string();
    if !matches!(op.as_str(), "+" | "-" | "*" | "/" | "%") {
        return Err(anyhow!("unknown op: {op} (one of + - * / %)"));
    }
    let decimals = opts.get("decimals").and_then(Value::as_i64);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let left = resolve_col(opts.get("left"), header_row)?;
    let constant = opts.get("value").and_then(Value::as_f64);
    let right = if constant.is_some() {
        None
    } else {
        Some(resolve_col(opts.get("right"), header_row)?)
    };
    if constant.is_none() && right.is_none() {
        return Err(anyhow!("need either right (column) or value (constant)"));
    }

    let apply = |l: f64, r: f64| -> Option<f64> {
        let v = match op.as_str() {
            "+" => l + r,
            "-" => l - r,
            "*" => l * r,
            "/" => {
                if r == 0.0 {
                    return None;
                }
                l / r
            }
            _ => {
                if r == 0.0 {
                    return None;
                }
                l % r
            }
        };
        Some(match decimals {
            Some(d) => {
                let f = 10f64.powi(d as i32);
                (v * f).round() / f
            }
            None => v,
        })
    };

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let new_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut cells = row.as_array().cloned().unwrap_or_default();
            if i < data_start {
                cells.push(json!(into));
                return Value::Array(cells);
            }
            let l = cells.get(left).and_then(sheet_cell_num);
            let r = match (constant, right) {
                (Some(c), _) => Some(c),
                (None, Some(rc)) => cells.get(rc).and_then(sheet_cell_num),
                _ => None,
            };
            let cell = match (l, r) {
                (Some(l), Some(r)) => apply(l, r).map_or(json!(""), |v| json!(v)),
                _ => json!(""),
            };
            cells.push(cell);
            Value::Array(cells)
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "column": into }))
}

/// Filter rows by multiple conditions (generalizes the single-predicate
/// `sheet_filter`). opts: path, output, conditions => array of
/// `{ column, op?, value, ignore_case? }` (op defaults to "eq"; see
/// `sheet_filter` for the op list), match => "all" (default, AND) | "any" (OR),
/// sheet, header (default true), format. Returns `{ ok, path, kept }`.
fn op_sheet_where(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let any = opts.get("match").and_then(Value::as_str) == Some("any");

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = sheet_target_index(&opts, &mut sheets)?;
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };

    let conds = opts
        .get("conditions")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing conditions (array of {{column, op?, value}})"))?;
    if conds.is_empty() {
        return Err(anyhow!("conditions must list at least one predicate"));
    }
    // (column index, op, value, ignore_case) per condition.
    let preds: Vec<(usize, String, Value, bool)> = conds
        .iter()
        .map(|c| {
            let col = resolve_col(c.get("column"), header_row)?;
            let op = c
                .get("op")
                .and_then(Value::as_str)
                .unwrap_or("eq")
                .to_string();
            let value = c.get("value").cloned().unwrap_or(Value::Null);
            let ic = c.get("ignore_case").and_then(flag_of).unwrap_or(false);
            Ok((col, op, value, ic))
        })
        .collect::<Result<_>>()?;

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut new_rows: Vec<Value> = Vec::new();
    if data_start == 1 {
        new_rows.push(rows[0].clone());
    }
    let mut kept = 0u64;
    for row in &rows[data_start..] {
        let test = |&(col, ref op, ref value, ic): &(usize, String, Value, bool)| {
            let cell = row
                .as_array()
                .and_then(|a| a.get(col))
                .unwrap_or(&Value::Null);
            cell_matches(cell, op, value, ic)
        };
        let pass = if any {
            preds.iter().any(test)
        } else {
            preds.iter().all(test)
        };
        if pass {
            new_rows.push(row.clone());
            kept += 1;
        }
    }
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "kept": kept }))
}

/// Explode a multi-sheet workbook into one file per sheet. opts: path,
/// dir => output directory, format => output extension (default: the source's),
/// prefix => optional filename prefix. Files are `{dir}/{prefix}{sheet}.{ext}`
/// with the sheet name sanitized. Returns `{ count, files }`.
fn op_sheet_split(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let dir = req_str(&opts, "dir")?;
    let ext = opts
        .get("format")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| ext_of(path));
    let prefix = opts.get("prefix").and_then(Value::as_str).unwrap_or("");

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut files = Vec::new();
    for s in &sheets {
        let name = s["name"].as_str().unwrap_or("Sheet");
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let safe = safe.trim();
        let out = format!("{dir}/{prefix}{safe}.{ext}");
        op_sheet_write(json!({ "path": out, "sheets": [s], "format": ext }))?;
        files.push(out);
    }
    Ok(json!({ "count": files.len(), "files": files }))
}

/// Split a sheet's rows into fixed-size chunks across files. opts: path,
/// dir => output directory, size => data rows per chunk (required, > 0),
/// header => repeat the first row in each chunk (default true), sheet,
/// format => output extension (default: source's), prefix => filename stem
/// (default: source's). Files are `{dir}/{prefix}-{n}.{ext}`. Returns
/// `{ count, files }`.
fn op_sheet_chunk(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let dir = req_str(&opts, "dir")?;
    let size = opts
        .get("size")
        .and_then(Value::as_u64)
        .filter(|&n| n > 0)
        .ok_or_else(|| anyhow!("missing size (rows per chunk, > 0)"))? as usize;
    let ext = opts
        .get("format")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| ext_of(path));
    let prefix = opts
        .get("prefix")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("chunk")
                .to_string()
        });
    let repeat_header = opts.get("header").and_then(flag_of).unwrap_or(true);

    let rows = select_sheet_rows(path, opts.get("sheet"))?;
    let (header, data) = if repeat_header && !rows.is_empty() {
        (Some(rows[0].clone()), &rows[1..])
    } else {
        (None, &rows[..])
    };

    let mut files = Vec::new();
    for (i, chunk) in data.chunks(size).enumerate() {
        let mut out_rows: Vec<Value> = Vec::with_capacity(chunk.len() + 1);
        if let Some(h) = &header {
            out_rows.push(h.clone());
        }
        out_rows.extend(chunk.iter().cloned());
        let out = format!("{dir}/{prefix}-{}.{ext}", i + 1);
        op_sheet_write(
            json!({ "path": out, "sheets": [{ "name": "Sheet1", "rows": out_rows }], "format": ext }),
        )?;
        files.push(out);
    }
    Ok(json!({ "count": files.len(), "files": files }))
}

/// Keep only the first (or last) N data rows of a sheet. opts: path, output,
/// n => row count (default 10), tail => bool (take the last N instead), header
/// => keep the first row (default true), sheet, format. Returns
/// `{ ok, path, rows }` (data rows kept).
fn op_sheet_head(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let n = opts.get("n").and_then(Value::as_u64).unwrap_or(10) as usize;
    let tail = opts.get("tail").and_then(flag_of).unwrap_or(false);
    let keep_header = opts.get("header").and_then(flag_of).unwrap_or(true);

    let rows = select_sheet_rows(path, opts.get("sheet"))?;
    let (header, data) = if keep_header && !rows.is_empty() {
        (Some(rows[0].clone()), &rows[1..])
    } else {
        (None, &rows[..])
    };
    let take: Vec<Value> = if tail {
        let mut v: Vec<Value> = data.iter().rev().take(n).cloned().collect();
        v.reverse();
        v
    } else {
        data.iter().take(n).cloned().collect()
    };

    let kept = take.len();
    let mut out_rows: Vec<Value> = Vec::with_capacity(kept + 1);
    if let Some(h) = header {
        out_rows.push(h);
    }
    out_rows.extend(take);

    let mut wopts = json!({ "path": output, "sheets": [{ "name": "Sheet1", "rows": out_rows }] });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "rows": kept }))
}

/// xorshift64 step — a tiny deterministic PRNG so sampling is reproducible
/// without pulling in a `rand` dependency. State must be non-zero.
fn xorshift64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Randomly sample N data rows (the header is always kept). opts: path, output,
/// n => sample size (default 10), seed => u64 PRNG seed for reproducibility
/// (default fixed), header => first row is a header to preserve (default true),
/// sheet, format. Sampling is without replacement; if `n` ≥ the row count every
/// row is kept. Sampled rows are emitted in their original order. Returns
/// `{ ok, path, rows }`.
fn op_sheet_sample(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let n = opts.get("n").and_then(Value::as_u64).unwrap_or(10) as usize;
    let keep_header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let seed = opts
        .get("seed")
        .and_then(Value::as_u64)
        .filter(|&s| s != 0)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);

    let rows = select_sheet_rows(path, opts.get("sheet"))?;
    let (header, data) = if keep_header && !rows.is_empty() {
        (Some(rows[0].clone()), &rows[1..])
    } else {
        (None, &rows[..])
    };

    // Assign each data row a pseudo-random key, take the n smallest keys, then
    // restore original order for a stable, reproducible sample.
    let mut state = seed;
    let mut keyed: Vec<(u64, usize)> = (0..data.len())
        .map(|i| (xorshift64(&mut state), i))
        .collect();
    keyed.sort_unstable();
    let mut picked: Vec<usize> = keyed.iter().take(n).map(|&(_, i)| i).collect();
    picked.sort_unstable();

    let mut out_rows: Vec<Value> = Vec::with_capacity(picked.len() + 1);
    if let Some(h) = header {
        out_rows.push(h);
    }
    for i in &picked {
        out_rows.push(data[*i].clone());
    }

    let kept = picked.len();
    let mut wopts = json!({ "path": output, "sheets": [{ "name": "Sheet1", "rows": out_rows }] });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "rows": kept }))
}

/// Title-case a string: capitalize the first letter of each whitespace-separated
/// word, lowercasing the rest.
fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Apply a named transform to every cell of one column (pandas `.str`/`.apply`
/// for common ops). opts: path, output, column => name or 0-based index
/// (required), op => one of upper|lower|trim|title (string) or
/// round|floor|ceil|abs|int (numeric; non-numeric cells pass through unchanged),
/// digits => decimals for `round` (default 0), into => append result as a new
/// column with this header (default: replace the column in place), sheet, header,
/// format. Returns `{ ok, path, transformed }` (count of data rows processed).
fn op_sheet_transform(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let op = req_str(&opts, "op")?.to_string();
    if !matches!(
        op.as_str(),
        "upper" | "lower" | "trim" | "title" | "round" | "floor" | "ceil" | "abs" | "int"
    ) {
        return Err(anyhow!("unknown op: {op}"));
    }
    let digits = opts.get("digits").and_then(Value::as_i64).unwrap_or(0) as i32;
    let into = opts.get("into").and_then(Value::as_str).map(str::to_string);

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let rows = sheets[target]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let header_row = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let col = resolve_col(opts.get("column"), header_row)?;

    let apply = |cell: &Value| -> Value {
        match op.as_str() {
            "upper" => json!(cell_to_string(cell).to_uppercase()),
            "lower" => json!(cell_to_string(cell).to_lowercase()),
            "trim" => json!(cell_to_string(cell).trim().to_string()),
            "title" => json!(title_case(&cell_to_string(cell))),
            _ => match sheet_cell_num(cell) {
                Some(x) => {
                    let y = match op.as_str() {
                        "round" => {
                            let f = 10f64.powi(digits);
                            (x * f).round() / f
                        }
                        "floor" => x.floor(),
                        "ceil" => x.ceil(),
                        "abs" => x.abs(),
                        "int" => x.trunc(),
                        _ => x,
                    };
                    json!(y)
                }
                None => cell.clone(),
            },
        }
    };

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut transformed = 0u64;
    let new_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut cells = row.as_array().cloned().unwrap_or_default();
            if i < data_start {
                if let Some(name) = &into {
                    cells.push(json!(name));
                }
                return Value::Array(cells);
            }
            let src = cells.get(col).cloned().unwrap_or(Value::Null);
            let result = apply(&src);
            transformed += 1;
            match &into {
                Some(_) => cells.push(result),
                None => {
                    if col < cells.len() {
                        cells[col] = result;
                    } else {
                        cells.resize(col + 1, Value::Null);
                        cells[col] = result;
                    }
                }
            }
            Value::Array(cells)
        })
        .collect();
    sheets[target]["rows"] = Value::Array(new_rows);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "transformed": transformed }))
}

/// Top-N rows by a column (sort then take N). opts: path, output, by => column
/// (required), n => row count (default 10), ascending => bool (default false =
/// largest first), numeric, sheet, header, format. Returns `{ ok, path, rows }`.
fn op_sheet_top(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let by = opts
        .get("by")
        .cloned()
        .ok_or_else(|| anyhow!("missing by (column name or index)"))?;
    let ascending = opts.get("ascending").and_then(flag_of).unwrap_or(false);

    // Sort base -> output, then keep the first N rows of output in place.
    let mut sopts = json!({ "path": path, "by": by, "output": output, "descending": !ascending });
    for k in ["sheet", "header", "numeric", "format"] {
        if let Some(v) = opts.get(k) {
            sopts[k] = v.clone();
        }
    }
    op_sheet_sort(sopts)?;

    let mut hopts = json!({ "path": output, "output": output });
    hopts["n"] = opts.get("n").cloned().unwrap_or_else(|| json!(10));
    for k in ["sheet", "header", "format"] {
        if let Some(v) = opts.get(k) {
            hopts[k] = v.clone();
        }
    }
    op_sheet_head(hopts)
}

/// Rename a sheet in a workbook. opts: path, output (default: in place),
/// from => sheet to rename (name or index; default first), to => new name
/// (required), format. Returns `{ ok, path, renamed }`.
fn op_sheet_rename(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let to = req_str(&opts, "to")?.to_string();

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let idx = match opts.get("from") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => Some(0),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;
    sheets[idx]["name"] = json!(to);

    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "renamed": to }))
}

/// Add a new sheet to a workbook. opts: path, output (default in place),
/// name => new sheet name (required), rows => [[cell,…]] (default empty),
/// position => 0-based insert index (default: append), format. Returns
/// `{ ok, path, sheets }` (new total).
fn op_sheet_add(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let name = req_str(&opts, "name")?.to_string();
    let rows = opts.get("rows").cloned().unwrap_or_else(|| json!([]));

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let new_sheet = json!({ "name": name, "rows": rows });
    match opts
        .get("position")
        .and_then(Value::as_u64)
        .map(|i| i as usize)
    {
        Some(p) if p <= sheets.len() => sheets.insert(p, new_sheet),
        _ => sheets.push(new_sheet),
    }

    let n = sheets.len();
    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "sheets": n }))
}

/// Remove a sheet from a workbook. opts: path, output (default in place),
/// sheet => the sheet to remove (name or index; required), format. Errors if it
/// would remove the only sheet. Returns `{ ok, path, removed, sheets }`.
fn op_sheet_remove(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();

    let read = op_sheet_read(json!({ "path": path }))?;
    let mut sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let idx = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().position(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().map(|i| i as usize),
        _ => return Err(anyhow!("missing sheet to remove (name or index)")),
    }
    .filter(|&i| i < sheets.len())
    .ok_or_else(|| anyhow!("sheet not found"))?;
    if sheets.len() <= 1 {
        return Err(anyhow!("cannot remove the only sheet"));
    }
    let removed = sheets[idx]["name"].as_str().unwrap_or("").to_string();
    sheets.remove(idx);

    let n = sheets.len();
    let mut wopts = json!({ "path": output, "sheets": sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "removed": removed, "sheets": n }))
}

/// Reorder (and/or subset) the sheets of a workbook. opts: path, output
/// (default in place), order => array of sheet names/indices in the desired
/// order (sheets omitted are dropped), format. Returns `{ ok, path, sheets }`.
fn op_sheet_reorder(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let order = opts
        .get("order")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing order (array of sheet names/indices)"))?;

    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out_sheets: Vec<Value> = Vec::new();
    for o in order {
        let idx = match o {
            Value::String(name) => sheets.iter().position(|s| s["name"] == *name),
            Value::Number(n) => n.as_u64().map(|i| i as usize),
            _ => None,
        }
        .filter(|&i| i < sheets.len());
        if let Some(i) = idx {
            out_sheets.push(sheets[i].clone());
        }
    }
    if out_sheets.is_empty() {
        return Err(anyhow!("order referenced no existing sheets"));
    }

    let n = out_sheets.len();
    let mut wopts = json!({ "path": output, "sheets": out_sheets });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_sheet_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "sheets": n }))
}

/// Read one sheet's rows from a file, selected by name/index (default first).
fn select_sheet_rows(path: &str, sel: Option<&Value>) -> Result<Vec<Value>> {
    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match sel {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;
    Ok(sheet["rows"].as_array().cloned().unwrap_or_default())
}

/// Workbook overview: name + dimensions of every sheet. opts: path. Returns
/// `{ count, sheets: [{ name, rows, cols }] }`.
fn op_sheet_info(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let info: Vec<Value> = sheets
        .iter()
        .map(|s| {
            let rows = s["rows"].as_array().map_or(0, Vec::len);
            let cols = s["rows"]
                .as_array()
                .map(|rs| {
                    rs.iter()
                        .map(|r| r.as_array().map_or(0, |a| a.len()))
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            json!({ "name": s["name"].as_str().unwrap_or(""), "rows": rows, "cols": cols })
        })
        .collect();
    Ok(json!({ "count": info.len(), "sheets": info }))
}

/// Universal file inspector: identify any office/image file by extension and
/// return a type-appropriate summary. opts: path. Returns `{ type, format,
/// path, … }` where the extra fields come from the matching `*_info`/`*_stats`
/// op (pdf → pages/geometry; spreadsheet → sheets; document → word counts;
/// presentation → slide stats; image → width/height).
fn op_info(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let ext = ext_of(path);
    let mut out = match ext.as_str() {
        "pdf" => {
            let mut r = op_pdf_info(json!({ "path": path }))?;
            r["type"] = json!("pdf");
            r
        }
        "xlsx" | "ods" | "xls" | "csv" | "tsv" => {
            let mut r = op_sheet_info(json!({ "path": path }))?;
            r["type"] = json!("spreadsheet");
            r
        }
        "docx" | "odt" | "rtf" | "md" | "markdown" | "html" | "htm" | "txt" => {
            let mut r = op_doc_stats(json!({ "path": path }))?;
            r["type"] = json!("document");
            r
        }
        "pptx" | "odp" => {
            let mut r = op_slides_stats(json!({ "path": path }))?;
            r["type"] = json!("presentation");
            r
        }
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "avif" | "ico" => {
            let mut r = op_img_open(json!({ "path": path }))?;
            if let Some(h) = r.get("handle").and_then(Value::as_u64) {
                let _ = op_img_close(json!({ "handle": h }));
            }
            if let Some(o) = r.as_object_mut() {
                o.remove("handle");
            }
            r["type"] = json!("image");
            r
        }
        other => return Err(anyhow!("unsupported file type: {other}")),
    };
    out["format"] = json!(ext);
    out["path"] = json!(path);
    Ok(out)
}

/// Compare two sheets cell by cell. opts: left, right (paths), sheet (selector
/// for both), left_sheet / right_sheet (override per side). Returns
/// `{ count, changed: [{ ref, row, col, left, right }], left_rows, right_rows }`
/// — cells whose string value differs (added/removed cells compare against
/// null), with 1-based row/col and an A1 `ref`.
fn op_sheet_diff(opts: Value) -> Result<Value> {
    let left = req_str(&opts, "left")?;
    let right = req_str(&opts, "right")?;
    let sel = opts.get("sheet");
    let lrows = select_sheet_rows(left, opts.get("left_sheet").or(sel))?;
    let rrows = select_sheet_rows(right, opts.get("right_sheet").or(sel))?;

    let cell = |rows: &[Value], r: usize, c: usize| -> Value {
        rows.get(r)
            .and_then(|x| x.as_array())
            .and_then(|a| a.get(c))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let width =
        |rows: &[Value], r: usize| rows.get(r).and_then(|x| x.as_array()).map_or(0, Vec::len);

    let nrows = lrows.len().max(rrows.len());
    let mut changed = Vec::new();
    for r in 0..nrows {
        let ncols = width(&lrows, r).max(width(&rrows, r));
        for c in 0..ncols {
            let lv = cell(&lrows, r, c);
            let rv = cell(&rrows, r, c);
            if cell_to_string(&lv) != cell_to_string(&rv) {
                changed.push(json!({
                    "ref": format!("{}{}", col_letters(c), r + 1),
                    "row": r + 1,
                    "col": c + 1,
                    "left": lv,
                    "right": rv,
                }));
            }
        }
    }
    Ok(json!({
        "count": changed.len(),
        "changed": changed,
        "left_rows": lrows.len(),
        "right_rows": rrows.len(),
    }))
}

/// Turn each spreadsheet row into a slide. opts: path, output (pptx/odp),
/// title_field => column whose value titles each slide (default: first column),
/// sheet, format. The remaining fields become `"field: value"` body lines.
/// Returns `{ ok, path, slides }`.
fn op_sheet_to_slides(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let recs = op_sheet_records(json!({ "path": path, "sheet": opts.get("sheet") }))?;
    let fields: Vec<String> = recs["fields"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let title_field = opts
        .get("title_field")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| fields.first().cloned().unwrap_or_default());
    let empty: Vec<Value> = Vec::new();
    let records = recs["records"].as_array().unwrap_or(&empty);

    let mut slides: Vec<Value> = Vec::new();
    for rec in records {
        let title = rec
            .get(&title_field)
            .map(cell_to_string)
            .unwrap_or_default();
        let body: Vec<Value> = fields
            .iter()
            .filter(|f| **f != title_field)
            .map(|f| {
                let v = cell_to_string(rec.get(f).unwrap_or(&Value::Null));
                json!(format!("{f}: {v}"))
            })
            .collect();
        slides.push(json!({ "title": title, "body": body }));
    }

    let mut wopts = json!({ "path": output, "slides": slides });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_slides_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "slides": records.len() }))
}

/// Validate a sheet's cells against per-column rules. opts: path, rules =>
/// [{ column (name/index), type? ("number"|"int"|"nonempty"), min?, max?,
/// allowed? [values] }], sheet, header (default true). Returns `{ valid, count,
/// violations: [{ ref, row, col, column, rule, value }] }`.
fn op_sheet_validate(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let rules = opts
        .get("rules")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing rules (expected array)"))?;
    let read = op_sheet_read(json!({ "path": path }))?;
    let sheets = read
        .get("sheets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sheet = match opts.get("sheet") {
        Some(Value::String(name)) => sheets.iter().find(|s| s["name"] == *name),
        Some(Value::Number(n)) => n.as_u64().and_then(|i| sheets.get(i as usize)),
        _ => sheets.first(),
    }
    .ok_or_else(|| anyhow!("sheet not found"))?;

    let empty: Vec<Value> = Vec::new();
    let rows = sheet["rows"].as_array().unwrap_or(&empty);
    let header = opts.get("header").and_then(flag_of).unwrap_or(true);
    let hr = if header {
        rows.first().and_then(Value::as_array).map(|v| v.as_slice())
    } else {
        None
    };
    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let cell = |row: &Value, c: usize| -> Value {
        row.as_array()
            .and_then(|a| a.get(c))
            .cloned()
            .unwrap_or(Value::Null)
    };

    let mut violations = Vec::new();
    for rule in rules {
        let col = resolve_col(rule.get("column"), hr)?;
        let cname = hr
            .and_then(|h| h.get(col))
            .map(cell_to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("Col{}", col + 1));
        for (ri, row) in rows[data_start..].iter().enumerate() {
            let c = cell(row, col);
            let blank = sheet_cell_blank(&c);
            let num = sheet_cell_num(&c);
            let mut bad: Option<String> = None;
            match rule.get("type").and_then(Value::as_str) {
                Some("nonempty") if blank => bad = Some("nonempty".into()),
                Some("number") if !blank && num.is_none() => bad = Some("number".into()),
                Some("int") if !blank && num.is_none_or(|x| x.fract() != 0.0) => {
                    bad = Some("int".into())
                }
                _ => {}
            }
            if bad.is_none() {
                if let Some(min) = rule.get("min").and_then(Value::as_f64) {
                    if num.is_some_and(|x| x < min) {
                        bad = Some(format!("min {min}"));
                    }
                }
            }
            if bad.is_none() {
                if let Some(max) = rule.get("max").and_then(Value::as_f64) {
                    if num.is_some_and(|x| x > max) {
                        bad = Some(format!("max {max}"));
                    }
                }
            }
            if bad.is_none() {
                if let Some(allowed) = rule.get("allowed").and_then(Value::as_array) {
                    let v = cell_to_string(&c);
                    if !blank && !allowed.iter().any(|a| cell_to_string(a) == v) {
                        bad = Some("allowed".into());
                    }
                }
            }
            if let Some(b) = bad {
                let rownum = data_start + ri + 1;
                violations.push(json!({
                    "ref": format!("{}{}", col_letters(col), rownum),
                    "row": rownum,
                    "col": col + 1,
                    "column": cname,
                    "rule": b,
                    "value": c,
                }));
            }
        }
    }
    Ok(json!({
        "valid": violations.is_empty(),
        "count": violations.len(),
        "violations": violations,
    }))
}

// ── word processing ──────────────────────────────────────────────────────────

fn op_doc_read(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let bytes = std::fs::read(path)?;
    let paragraphs = match ext_of(path).as_str() {
        "docx" => {
            let xml = read_zip_entry(&bytes, "word/document.xml")?;
            extract_paragraphs(&xml, &["w:p"])
        }
        "odt" => {
            let xml = read_zip_entry(&bytes, "content.xml")?;
            extract_paragraphs(&xml, &["text:p", "text:h"])
        }
        e @ ("html" | "htm" | "md" | "markdown" | "txt" | "rtf") => {
            return read_doc_text(path, e);
        }
        other => return Err(anyhow!("unsupported document read format: {other}")),
    };
    Ok(json!({ "paragraphs": paragraphs }))
}

fn blocks_of(opts: &Value) -> Result<Vec<Value>> {
    Ok(opts
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing blocks (expected array)"))?
        .clone())
}

/// Plain text of a block (concatenating any styled runs) — for the ODF path,
/// whose serializer is unstyled.
fn block_plain_text(b: &Value) -> String {
    if let Some(runs) = b.get("runs").and_then(Value::as_array) {
        runs.iter()
            .filter_map(|r| r.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("")
    } else if let Some(t) = b.get("text").and_then(Value::as_str) {
        t.to_string()
    } else if b.is_object() {
        // An object block with no text/runs (e.g. a structural wrapper).
        String::new()
    } else {
        // A bare scalar cell (string/number/bool), e.g. a table cell.
        cell_to_string(b)
    }
}

/// Build a formatted docx Run from `{text, bold, italic, underline, strike,
/// size (pt), color, font, highlight}`.
fn docx_run(j: &Value) -> docx_rs::Run {
    use docx_rs::{Run, RunFonts};
    let mut run = Run::new().add_text(j.get("text").and_then(Value::as_str).unwrap_or(""));
    if j.get("bold").and_then(flag_of) == Some(true) {
        run = run.bold();
    }
    if j.get("italic").and_then(flag_of) == Some(true) {
        run = run.italic();
    }
    if j.get("strike").and_then(flag_of) == Some(true) {
        run = run.strike();
    }
    if j.get("underline").and_then(flag_of) == Some(true) {
        run = run.underline("single");
    }
    if let Some(sz) = j.get("size").and_then(Value::as_f64) {
        run = run.size((sz * 2.0) as usize); // docx sz is half-points
    }
    if let Some(c) = j.get("color").and_then(Value::as_str) {
        run = run.color(c.trim_start_matches('#'));
    }
    if let Some(h) = j.get("highlight").and_then(Value::as_str) {
        run = run.highlight(h);
    }
    if let Some(f) = j.get("font").and_then(Value::as_str) {
        run = run.fonts(RunFonts::new().ascii(f));
    }
    run
}

fn docx_align(a: &str) -> docx_rs::AlignmentType {
    use docx_rs::AlignmentType;
    match a {
        "center" => AlignmentType::Center,
        "right" => AlignmentType::Right,
        "both" | "justify" | "justified" => AlignmentType::Both,
        _ => AlignmentType::Left,
    }
}

/// Build a docx Paragraph from a value: styled `runs`, a single `{text,...}`,
/// or a bare scalar.
fn docx_para(v: &Value) -> docx_rs::Paragraph {
    use docx_rs::{Paragraph, Run};
    let mut p = Paragraph::new();
    match v {
        Value::Object(o) if o.contains_key("runs") => {
            if let Some(runs) = o.get("runs").and_then(Value::as_array) {
                for rj in runs {
                    p = p.add_run(docx_run(rj));
                }
            }
        }
        Value::Object(_) => p = p.add_run(docx_run(v)),
        other => p = p.add_run(Run::new().add_text(cell_to_string(other))),
    }
    if let Some(a) = v.get("align").and_then(Value::as_str) {
        p = p.align(docx_align(a));
    }
    p
}

/// Write docx with rich runs + alignment, plus tables, inline images, page
/// breaks, and page setup. Blocks:
///   `{kind:"para"|"heading", level?, align?, text?|runs?}`
///   `{kind:"table", rows:[[cell,...]]}`  (cell = text or `{text|runs,...}`)
///   `{kind:"image", path, width?, height?}`  (px)
///   `{kind:"pagebreak"}`
/// Doc opts: `page_size:[w,h]` (twips).
fn write_docx(path: &str, blocks: &[Value], opts: &Value) -> Result<()> {
    use docx_rs::{
        AbstractNumbering, BreakType, Docx, Footer, Header, Hyperlink, HyperlinkType, IndentLevel,
        Level, LevelJc, LevelText, NumberFormat, Numbering, NumberingId, PageNum, Paragraph, Pic,
        Run, Shading, Start, Table, TableCell, TableRow, VAlignType, WidthType,
    };
    let mut docx = Docx::new();
    if let Some(ps) = opts.get("page_size").and_then(Value::as_array) {
        if let (Some(w), Some(h)) = (
            ps.first().and_then(Value::as_u64),
            ps.get(1).and_then(Value::as_u64),
        ) {
            docx = docx.page_size(w as u32, h as u32);
        }
    }
    // Register numbering definitions once if any list block is present:
    // num 1 = ordered (decimal), num 2 = bulleted.
    let has_list = blocks
        .iter()
        .any(|b| b.get("kind").and_then(Value::as_str) == Some("list"));
    if has_list {
        let lvl = |fmt: &str, text: &str| {
            Level::new(
                0,
                Start::new(1),
                NumberFormat::new(fmt),
                LevelText::new(text),
                LevelJc::new("left"),
            )
        };
        docx = docx
            .add_abstract_numbering(AbstractNumbering::new(1).add_level(lvl("decimal", "%1.")))
            .add_numbering(Numbering::new(1, 1))
            .add_abstract_numbering(AbstractNumbering::new(2).add_level(lvl("bullet", "•")))
            .add_numbering(Numbering::new(2, 2));
    }
    // Optional running header/footer (plain text).
    if let Some(h) = opts.get("header").and_then(Value::as_str) {
        docx = docx
            .header(Header::new().add_paragraph(Paragraph::new().add_run(Run::new().add_text(h))));
    }
    // Footer: optional text and/or an automatic page-number field.
    let page_numbers = opts.get("page_numbers").and_then(flag_of).unwrap_or(false);
    let footer_text = opts.get("footer").and_then(Value::as_str);
    if footer_text.is_some() || page_numbers {
        let mut fpara = Paragraph::new();
        if let Some(f) = footer_text {
            fpara = fpara.add_run(Run::new().add_text(f));
        }
        if page_numbers {
            fpara = fpara.add_page_num(PageNum::new());
        }
        docx = docx.footer(Footer::new().add_paragraph(fpara));
    }
    for b in blocks {
        match b.get("kind").and_then(Value::as_str).unwrap_or("para") {
            "list" => {
                let ordered = b.get("ordered").and_then(flag_of).unwrap_or(false);
                let num_id = if ordered { 1 } else { 2 };
                if let Some(items) = b.get("items").and_then(Value::as_array) {
                    for it in items {
                        docx = docx.add_paragraph(
                            Paragraph::new()
                                .add_run(Run::new().add_text(cell_to_string(it)))
                                .numbering(NumberingId::new(num_id), IndentLevel::new(0)),
                        );
                    }
                }
            }
            "link" => {
                let url = req_str(b, "url")?;
                let text = b.get("text").and_then(Value::as_str).unwrap_or(url);
                let hl = Hyperlink::new(url, HyperlinkType::External).add_run(
                    Run::new()
                        .add_text(text)
                        .color("0563C1")
                        .underline("single"),
                );
                docx = docx.add_paragraph(Paragraph::new().add_hyperlink(hl));
            }
            "table" => {
                let mut trows = Vec::new();
                if let Some(rows) = b.get("rows").and_then(Value::as_array) {
                    for row in rows {
                        let mut cells = Vec::new();
                        if let Some(rc) = row.as_array() {
                            for cell in rc {
                                // Styled cell: object keys bg, span, width (dxa),
                                // valign ("top"/"center"/"bottom").
                                let mut tc = TableCell::new().add_paragraph(docx_para(cell));
                                if let Some(o) = cell.as_object() {
                                    if let Some(bg) = o.get("bg").and_then(Value::as_str) {
                                        tc = tc.shading(
                                            Shading::new().fill(bg.trim_start_matches('#')),
                                        );
                                    }
                                    if let Some(span) = o.get("span").and_then(Value::as_u64) {
                                        tc = tc.grid_span(span as usize);
                                    }
                                    if let Some(w) = o.get("width").and_then(Value::as_u64) {
                                        tc = tc.width(w as usize, WidthType::Dxa);
                                    }
                                    if let Some(va) = o.get("valign").and_then(Value::as_str) {
                                        tc = tc.vertical_align(match va {
                                            "top" => VAlignType::Top,
                                            "bottom" => VAlignType::Bottom,
                                            _ => VAlignType::Center,
                                        });
                                    }
                                }
                                cells.push(tc);
                            }
                        }
                        trows.push(TableRow::new(cells));
                    }
                }
                docx = docx.add_table(Table::new(trows));
            }
            "image" => {
                let img_path = req_str(b, "path")?;
                let bytes = std::fs::read(img_path)?;
                let mut pic = Pic::new(&bytes);
                if let (Some(w), Some(h)) = (
                    b.get("width").and_then(Value::as_u64),
                    b.get("height").and_then(Value::as_u64),
                ) {
                    // px -> EMU (914400 EMU/inch at 96 dpi = 9525 EMU/px)
                    pic = pic.size(w as u32 * 9525, h as u32 * 9525);
                }
                docx = docx.add_paragraph(Paragraph::new().add_run(Run::new().add_image(pic)));
            }
            "pagebreak" => {
                docx = docx
                    .add_paragraph(Paragraph::new().add_run(Run::new().add_break(BreakType::Page)));
            }
            kind => {
                let mut p = docx_para(b);
                if kind == "heading" {
                    let level = b
                        .get("level")
                        .and_then(Value::as_u64)
                        .unwrap_or(1)
                        .clamp(1, 9);
                    p = p.style(&format!("Heading{level}"));
                }
                docx = docx.add_paragraph(p);
            }
        }
    }
    let file = std::fs::File::create(path)?;
    docx.build().pack(file)?;
    Ok(())
}

/// Write odt. lo_odf's serializer is unstyled, so rich runs are flattened to
/// plain text (per-run formatting is a documented ODF-write limitation).
fn write_odt(path: &str, blocks: &[Value]) -> Result<()> {
    use lo_core::TextDocument;
    let mut doc = TextDocument::new("stryke-office");
    for b in blocks {
        let text = block_plain_text(b);
        if b.get("kind").and_then(Value::as_str) == Some("heading") {
            let level = b.get("level").and_then(Value::as_u64).unwrap_or(1) as u8;
            doc.push_heading(level, text);
        } else {
            doc.push_paragraph(text);
        }
    }
    lo_odf::save_text_document(path, &doc)?;
    Ok(())
}

fn op_doc_write(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?.to_string();
    let blocks = blocks_of(&opts)?;
    match target_ext(&opts, &path).as_str() {
        "docx" => write_docx(&path, &blocks, &opts)?,
        "odt" => write_odt(&path, &blocks)?,
        "html" | "htm" => write_doc_html(&path, &blocks)?,
        "md" | "markdown" => write_doc_md(&path, &blocks)?,
        "rtf" => write_doc_rtf(&path, &blocks)?,
        "txt" => std::fs::write(
            &path,
            blocks
                .iter()
                .map(block_plain_text)
                .collect::<Vec<_>>()
                .join("\n"),
        )?,
        "pdf" => {
            // Flatten blocks to lines and emit a self-contained PDF
            // (headings padded with a blank line). lo_core supplies the font.
            let mut lines: Vec<String> = Vec::new();
            for b in &blocks {
                let kind = b.get("kind").and_then(Value::as_str).unwrap_or("para");
                if kind == "pagebreak" {
                    lines.push(String::new());
                    continue;
                }
                lines.push(block_plain_text(b));
                if kind == "heading" {
                    lines.push(String::new());
                }
            }
            let bytes = lo_core::write_text_pdf(
                &lines,
                lo_core::units::Length::mm(210.0),
                lo_core::units::Length::mm(297.0),
            );
            std::fs::write(&path, bytes)?;
        }
        other => return Err(anyhow!("unsupported document write format: {other}")),
    }
    Ok(json!({"ok": true, "path": path, "blocks": blocks.len()}))
}

// ── presentations ────────────────────────────────────────────────────────────

fn slide_index(name: &str) -> u32 {
    name.trim_start_matches("ppt/slides/slide")
        .trim_end_matches(".xml")
        .parse()
        .unwrap_or(0)
}

/// Crudely split an ODF presentation content.xml into per-`draw:page` chunks.
fn split_draw_pages(xml: &[u8]) -> Vec<Vec<u8>> {
    let s = String::from_utf8_lossy(xml);
    let mut pages = Vec::new();
    let mut rest = s.as_ref();
    while let Some(start) = rest.find("<draw:page") {
        let after = &rest[start..];
        if let Some(end) = after.find("</draw:page>") {
            pages.push(after.as_bytes()[..end].to_vec());
            rest = &after[end + "</draw:page>".len()..];
        } else {
            break;
        }
    }
    pages
}

/// The zip path of a pptx slide's speaker-notes part, resolved through the
/// slide's `.rels` (`ppt/slides/_rels/slideN.xml.rels`). `None` if the slide
/// has no notes relationship.
fn pptx_notes_part(bytes: &[u8], slide_name: &str) -> Option<String> {
    let (dir, file) = slide_name.rsplit_once('/')?;
    let rels = read_zip_entry(bytes, &format!("{dir}/_rels/{file}.rels")).ok()?;
    let target = rels_relationship_target(&rels, "notesSlide")?;
    Some(resolve_zip_path(dir, &target))
}

/// Split an odp `draw:page`'s `text:p` paragraphs into (slide text, notes text)
/// by whether each sits inside a `presentation:notes` subtree. Also keeps the
/// slide's own text free of notes — extracting `text:p` over the whole page
/// would otherwise fold the notes into the slide body.
fn odp_text_and_notes(page: &[u8]) -> (Vec<String>, Vec<String>) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(page);
    let mut buf = Vec::new();
    let (mut main, mut notes) = (Vec::new(), Vec::new());
    let mut in_notes = 0i32;
    let mut in_para = false;
    let mut cur = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"presentation:notes" => in_notes += 1,
                b"text:p" => {
                    in_para = true;
                    cur.clear();
                }
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if in_para {
                    if let Ok(t) = e.xml10_content() {
                        cur.push_str(&t);
                    }
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"text:p" if in_para => {
                    in_para = false;
                    let s = std::mem::take(&mut cur);
                    if in_notes > 0 {
                        notes.push(s);
                    } else {
                        main.push(s);
                    }
                }
                b"presentation:notes" => in_notes -= 1,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    (main, notes)
}

fn op_slides_read(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let bytes = std::fs::read(path)?;
    let mut slides = Vec::new();
    match ext_of(path).as_str() {
        "pptx" => {
            let mut names: Vec<String> = zip_entry_names(&bytes)?
                .into_iter()
                .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
                .collect();
            names.sort_by_key(|n| slide_index(n));
            for n in names {
                let xml = read_zip_entry(&bytes, &n)?;
                let text = extract_paragraphs(&xml, &["a:p"]);
                let notes = pptx_notes_part(&bytes, &n)
                    .and_then(|p| read_zip_entry(&bytes, &p).ok())
                    .map(|nx| extract_paragraphs(&nx, &["a:p"]))
                    .unwrap_or_default();
                slides.push(json!({ "text": text, "notes": notes }));
            }
        }
        "odp" => {
            let xml = read_zip_entry(&bytes, "content.xml")?;
            for page in split_draw_pages(&xml) {
                let (text, notes) = odp_text_and_notes(&page);
                slides.push(json!({ "text": text, "notes": notes }));
            }
        }
        other => return Err(anyhow!("unsupported presentation read format: {other}")),
    }
    Ok(json!({ "slides": slides }))
}

/// A slide spec: {title?: string, body?: [string]}.
/// A slide spec: title + body items (each a string or `{text, bold, italic,
/// size, color}`). Body items stay raw so the pptx writer can apply run
/// formatting; the ODF writer flattens them to plain text.
fn json_slides(opts: &Value) -> Result<Vec<(String, Vec<Value>)>> {
    let arr = opts
        .get("slides")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing slides (expected array)"))?;
    Ok(arr
        .iter()
        .map(|s| {
            let title = s
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let body = s
                .get("body")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            (title, body)
        })
        .collect())
}

/// Plain text of a slide body item (string or `{text}`).
fn slide_item_text(v: &Value) -> String {
    match v {
        Value::Object(o) => cell_to_string(o.get("text").unwrap_or(&Value::Null)),
        other => cell_to_string(other),
    }
}

fn write_odp(path: &str, slides: &[(String, Vec<Value>)]) -> Result<()> {
    use lo_core::geometry::Rect;
    use lo_core::impress::{Slide, SlideElement, TextBox};
    use lo_core::style::TextBoxStyle;
    use lo_core::units::Length;
    use lo_core::Presentation;
    let mut pres = Presentation::new("stryke-office");
    for (title, body) in slides {
        let mut slide = Slide {
            name: title.clone(),
            ..Slide::default()
        };
        let mut text = title.clone();
        if !body.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            let lines: Vec<String> = body.iter().map(slide_item_text).collect();
            text.push_str(&lines.join("\n"));
        }
        slide.elements.push(SlideElement::TextBox(TextBox {
            frame: Rect::new(
                Length::mm(20.0),
                Length::mm(20.0),
                Length::mm(240.0),
                Length::mm(120.0),
            ),
            text,
            style: TextBoxStyle::default(),
        }));
        pres.slides.push(slide);
    }
    lo_odf::save_presentation_document(path, &pres)?;
    Ok(())
}

fn op_slides_write(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?.to_string();
    let slides = json_slides(&opts)?;
    match target_ext(&opts, &path).as_str() {
        "odp" => write_odp(&path, &slides)?,
        "pptx" => write_pptx(&path, &slides)?,
        other => return Err(anyhow!("unsupported presentation write format: {other}")),
    }
    Ok(json!({"ok": true, "path": path, "slides": slides.len()}))
}

/// Read every file entry of a zip container into `(name, bytes)`, skipping
/// directory entries. Used to rewrite OOXML packages part-by-part.
fn read_zip_entries(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let name = f.name().to_string();
        if name.ends_with('/') {
            continue;
        }
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        out.push((name, buf));
    }
    Ok(out)
}

/// Write `(name, bytes)` entries to a new deflated zip, returning the archive.
fn write_zip_entries(entries: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let zopt = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        zw.start_file(name.as_str(), zopt)?;
        zw.write_all(bytes)?;
    }
    Ok(zw.finish()?.into_inner())
}

/// Image content type for a file extension (PresentationML media).
fn image_content_type(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        _ => "application/octet-stream",
    }
}

/// Insert a picture onto a slide of an existing pptx (the deck analogue of
/// `pdf_stamp_image`). Embeds the image as a `ppt/media` part, adds the slide
/// relationship + content-type, and injects a `p:pic` shape into the slide's
/// shape tree. opts: path (pptx), image => image file (png/jpeg/gif/bmp/tiff),
/// output => target (default in place), slide => 1-based slide number (default
/// 1), x/y => offset in pixels (default 96 = 1 inch), width/height => size in
/// pixels (default the image's native size). Pixels convert to EMU at 96 dpi.
/// Returns `{ ok, path, slide, image }`.
fn op_slides_add_image(opts: Value) -> Result<Value> {
    const EMU_PER_PX: i64 = 9525;
    let path = req_str(&opts, "path")?;
    let image = req_str(&opts, "image")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let slide_no = opts.get("slide").and_then(Value::as_u64).unwrap_or(1);
    let ext = ext_of(image);
    if image_content_type(&ext) == "application/octet-stream" {
        return Err(anyhow!("unsupported image format: {ext}"));
    }

    let (iw, ih) =
        image::image_dimensions(image).map_err(|e| anyhow!("read image {image}: {e}"))?;
    let x = opts.get("x").and_then(Value::as_i64).unwrap_or(96) * EMU_PER_PX;
    let y = opts.get("y").and_then(Value::as_i64).unwrap_or(96) * EMU_PER_PX;
    let cx = opts
        .get("width")
        .and_then(Value::as_i64)
        .unwrap_or(iw as i64)
        * EMU_PER_PX;
    let cy = opts
        .get("height")
        .and_then(Value::as_i64)
        .unwrap_or(ih as i64)
        * EMU_PER_PX;
    let img_bytes = std::fs::read(image)?;

    // Read every entry of the source pptx into memory.
    let mut entries = read_zip_entries(&std::fs::read(path)?)?;

    let slide_name = format!("ppt/slides/slide{slide_no}.xml");
    if !entries.iter().any(|(n, _)| n == &slide_name) {
        return Err(anyhow!("slide {slide_no} not found in {path}"));
    }

    // Next free media index and image part name.
    let mut max_img = 0u32;
    for (n, _) in &entries {
        if let Some(rest) = n.strip_prefix("ppt/media/image") {
            let num: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(v) = num.parse::<u32>() {
                max_img = max_img.max(v);
            }
        }
    }
    let img_idx = max_img + 1;
    let media_name = format!("ppt/media/image{img_idx}.{ext}");

    // Slide rels: append an image relationship, picking the next rId.
    let rels_name = format!("ppt/slides/_rels/slide{slide_no}.xml.rels");
    let rel_target = format!("../media/image{img_idx}.{ext}");
    let rel = |rid: u32| {
        format!(
            "<Relationship Id=\"rId{rid}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"{rel_target}\"/>"
        )
    };
    let mut rid = 1u32;
    if let Some((_, bytes)) = entries.iter_mut().find(|(n, _)| n == &rels_name) {
        let s = String::from_utf8_lossy(bytes).into_owned();
        for cap in s.split("Id=\"rId").skip(1) {
            let num: String = cap.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(v) = num.parse::<u32>() {
                rid = rid.max(v + 1);
            }
        }
        let new_s = s.replacen(
            "</Relationships>",
            &format!("{}</Relationships>", rel(rid)),
            1,
        );
        *bytes = new_s.into_bytes();
    } else {
        let rels = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{}</Relationships>\n",
            rel(rid)
        );
        entries.push((rels_name, rels.into_bytes()));
    }

    // Inject a p:pic shape into the slide's shape tree.
    let pic = format!(
        "<p:pic><p:nvPicPr><p:cNvPr id=\"{cid}\" name=\"Picture {img_idx}\"/><p:cNvPicPr><a:picLocks noChangeAspect=\"1\"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed=\"rId{rid}\"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr></p:pic>",
        cid = 1000 + img_idx,
    );
    if let Some((_, bytes)) = entries.iter_mut().find(|(n, _)| n == &slide_name) {
        let s = String::from_utf8_lossy(bytes).into_owned();
        let new_s = s.replacen("</p:spTree>", &format!("{pic}</p:spTree>"), 1);
        *bytes = new_s.into_bytes();
    }

    // Ensure [Content_Types].xml declares the image extension.
    if let Some((_, bytes)) = entries.iter_mut().find(|(n, _)| n == "[Content_Types].xml") {
        let s = String::from_utf8_lossy(bytes).into_owned();
        if !s.contains(&format!("Extension=\"{ext}\"")) {
            let decl = format!(
                "<Default Extension=\"{ext}\" ContentType=\"{}\"/>",
                image_content_type(&ext)
            );
            let new_s = s.replacen("</Types>", &format!("{decl}</Types>"), 1);
            *bytes = new_s.into_bytes();
        }
    }

    // Add the media part and rewrite the package.
    entries.push((media_name, img_bytes));
    std::fs::write(&output, write_zip_entries(&entries)?)?;

    Ok(json!({ "ok": true, "path": output, "slide": slide_no, "image": image }))
}

/// Set (or replace) the speaker notes on a slide of an existing pptx. The
/// reader (`slides_read`) recovers notes via the slide's `notesSlide`
/// relationship, so this writes a minimal `ppt/notesSlides` part, links it from
/// the slide rels, and declares its content type. opts: path (pptx), slide =>
/// 1-based slide number (default 1), notes => a string (split on newlines) or an
/// array of lines, output => target (default in place). Returns `{ ok, path,
/// slide, lines }`.
fn op_slides_set_notes(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let slide_no = opts.get("slide").and_then(Value::as_u64).unwrap_or(1);
    let lines: Vec<String> = match opts.get("notes") {
        Some(Value::Array(a)) => a.iter().map(cell_to_string).collect(),
        Some(Value::String(s)) => s.lines().map(str::to_string).collect(),
        Some(other) => vec![cell_to_string(other)],
        None => return Err(anyhow!("missing notes (string or array of lines)")),
    };

    let mut entries = read_zip_entries(&std::fs::read(path)?)?;
    let slide_name = format!("ppt/slides/slide{slide_no}.xml");
    if !entries.iter().any(|(n, _)| n == &slide_name) {
        return Err(anyhow!("slide {slide_no} not found in {path}"));
    }

    let paras: String = lines
        .iter()
        .map(|l| {
            format!(
                "<a:p><a:r><a:rPr lang=\"en-US\"/><a:t>{}</a:t></a:r></a:p>",
                xml_escape(l)
            )
        })
        .collect();
    let notes_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:notes xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/><p:sp><p:nvSpPr><p:cNvPr id=\"2\" name=\"Notes Placeholder\"/><p:cNvSpPr><a:spLocks noGrp=\"1\"/></p:cNvSpPr><p:nvPr><p:ph type=\"body\" idx=\"1\"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/>{paras}</p:txBody></p:sp></p:spTree></p:cSld></p:notes>\n"
    );

    let rels_name = format!("ppt/slides/_rels/slide{slide_no}.xml.rels");
    let existing_notes = entries
        .iter()
        .find(|(n, _)| n == &rels_name)
        .and_then(|(_, b)| {
            let s = String::from_utf8_lossy(b);
            rels_relationship_target(s.as_bytes(), "notesSlide")
                .map(|t| resolve_zip_path("ppt/slides", &t))
        });

    if let Some(notes_part) = existing_notes {
        // Replace the body of the already-linked notesSlide part.
        if let Some((_, bytes)) = entries.iter_mut().find(|(n, _)| *n == notes_part) {
            *bytes = notes_xml.into_bytes();
        }
    } else {
        // Allocate a fresh notesSlide index.
        let mut max_idx = 0u32;
        for (n, _) in &entries {
            if let Some(rest) = n.strip_prefix("ppt/notesSlides/notesSlide") {
                let num: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(v) = num.parse::<u32>() {
                    max_idx = max_idx.max(v);
                }
            }
        }
        let idx = max_idx + 1;
        let notes_part = format!("ppt/notesSlides/notesSlide{idx}.xml");

        // Link it from the slide rels (creating the rels part if absent).
        let rel = |rid: u32| {
            format!(
                "<Relationship Id=\"rId{rid}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide\" Target=\"../notesSlides/notesSlide{idx}.xml\"/>"
            )
        };
        if let Some((_, bytes)) = entries.iter_mut().find(|(n, _)| n == &rels_name) {
            let s = String::from_utf8_lossy(bytes).into_owned();
            let mut rid = 1u32;
            for cap in s.split("Id=\"rId").skip(1) {
                let num: String = cap.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(v) = num.parse::<u32>() {
                    rid = rid.max(v + 1);
                }
            }
            *bytes = s
                .replacen(
                    "</Relationships>",
                    &format!("{}</Relationships>", rel(rid)),
                    1,
                )
                .into_bytes();
        } else {
            let rels = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{}</Relationships>\n",
                rel(1)
            );
            entries.push((rels_name, rels.into_bytes()));
        }

        // The notesSlide's own rels point back at the slide.
        let notes_rels = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"../slides/slide{slide_no}.xml\"/></Relationships>\n"
        );
        entries.push((
            format!("ppt/notesSlides/_rels/notesSlide{idx}.xml.rels"),
            notes_rels.into_bytes(),
        ));

        // Declare the new part's content type.
        if let Some((_, bytes)) = entries.iter_mut().find(|(n, _)| n == "[Content_Types].xml") {
            let s = String::from_utf8_lossy(bytes).into_owned();
            let decl = format!(
                "<Override PartName=\"/{notes_part}\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml\"/>"
            );
            *bytes = s
                .replacen("</Types>", &format!("{decl}</Types>"), 1)
                .into_bytes();
        }

        entries.push((notes_part, notes_xml.into_bytes()));
    }

    std::fs::write(&output, write_zip_entries(&entries)?)?;
    Ok(json!({ "ok": true, "path": output, "slide": slide_no, "lines": lines.len() }))
}

/// Add a text box to a slide of an existing pptx (the text analogue of
/// `slides_add_image`). Injects a `p:sp` shape into the slide's shape tree, so
/// the text is recoverable via `slides_read`. opts: path, text (required), slide
/// => 1-based slide number (default 1), x/y => offset in pixels (default 96),
/// width/height => box size in pixels (default 400×100), size => font size in pt
/// (default 18), output => target (default in place). Pixels convert to EMU at
/// 96 dpi. Returns `{ ok, path, slide }`.
fn op_slides_add_text(opts: Value) -> Result<Value> {
    const EMU_PER_PX: i64 = 9525;
    let path = req_str(&opts, "path")?;
    let text = req_str(&opts, "text")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let slide_no = opts.get("slide").and_then(Value::as_u64).unwrap_or(1);
    let x = opts.get("x").and_then(Value::as_i64).unwrap_or(96) * EMU_PER_PX;
    let y = opts.get("y").and_then(Value::as_i64).unwrap_or(96) * EMU_PER_PX;
    let cx = opts.get("width").and_then(Value::as_i64).unwrap_or(400) * EMU_PER_PX;
    let cy = opts.get("height").and_then(Value::as_i64).unwrap_or(100) * EMU_PER_PX;
    let sz = opts.get("size").and_then(Value::as_i64).unwrap_or(18) * 100;

    let mut entries = read_zip_entries(&std::fs::read(path)?)?;
    let slide_name = format!("ppt/slides/slide{slide_no}.xml");
    if !entries.iter().any(|(n, _)| n == &slide_name) {
        return Err(anyhow!("slide {slide_no} not found in {path}"));
    }

    let sp = format!(
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"TextBox {slide_no}\"/><p:cNvSpPr txBox=\"1\"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang=\"en-US\" sz=\"{sz}\"/><a:t>{t}</a:t></a:r></a:p></p:txBody></p:sp>",
        id = 5000 + slide_no,
        t = xml_escape(text),
    );
    if let Some((_, bytes)) = entries.iter_mut().find(|(n, _)| *n == slide_name) {
        let s = String::from_utf8_lossy(bytes).into_owned();
        *bytes = s
            .replacen("</p:spTree>", &format!("{sp}</p:spTree>"), 1)
            .into_bytes();
    }

    std::fs::write(&output, write_zip_entries(&entries)?)?;
    Ok(json!({ "ok": true, "path": output, "slide": slide_no }))
}

/// Concatenate presentations into one deck. opts: inputs => [paths],
/// output => path, format => override. Each source slide's first text line
/// becomes the title and the rest the body; the target format follows the
/// output extension (so merge also converts pptx<->odp). Note: only slide text
/// is carried (the write side does not emit speaker notes or media). Returns
/// `{ ok, path, sources, slides }`.
fn op_slides_merge(opts: Value) -> Result<Value> {
    let inputs = opts
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing inputs (expected array of paths)"))?;
    let output = req_str(&opts, "output")?.to_string();

    let mut out_slides: Vec<Value> = Vec::new();
    for inp in inputs {
        let path = inp
            .as_str()
            .ok_or_else(|| anyhow!("input path must be a string"))?;
        let read = op_slides_read(json!({ "path": path }))?;
        let slides = read
            .get("slides")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for s in &slides {
            let text: Vec<String> = s
                .get("text")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let (title, body) = text
                .split_first()
                .map(|(h, rest)| (h.clone(), rest.to_vec()))
                .unwrap_or_default();
            out_slides.push(json!({ "title": title, "body": body }));
        }
    }

    let n = out_slides.len();
    let mut wopts = json!({ "path": output, "slides": out_slides });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_slides_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "sources": inputs.len(), "slides": n }))
}

/// Reorder (and/or subset) a deck's slides by a 1-based order list — the deck
/// analogue of `pdf_reorder`. opts: path (pptx/odp), order => array of 1-based
/// slide numbers in the desired order (slides omitted are dropped; required),
/// output (default in place), format. Each slide's first text line becomes the
/// title and the rest the body (speaker notes are not carried). Returns
/// `{ ok, path, slides }`.
fn op_slides_reorder(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let order: Vec<usize> = opts
        .get("order")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing order (array of 1-based slide numbers)"))?
        .iter()
        .filter_map(|v| v.as_u64().map(|n| n as usize))
        .collect();
    if order.is_empty() {
        return Err(anyhow!("order must list at least one slide"));
    }

    let read = op_slides_read(json!({ "path": path }))?;
    let slides = read
        .get("slides")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let as_spec = |s: &Value| -> Value {
        let text: Vec<String> = s
            .get("text")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let (title, body) = text
            .split_first()
            .map(|(h, rest)| (h.clone(), rest.to_vec()))
            .unwrap_or_default();
        json!({ "title": title, "body": body })
    };
    let out_slides: Vec<Value> = order
        .iter()
        .filter_map(|&n| slides.get(n.checked_sub(1)?).map(as_spec))
        .collect();
    if out_slides.is_empty() {
        return Err(anyhow!("order referenced no existing slides"));
    }

    let n = out_slides.len();
    let mut wopts = json!({ "path": output, "slides": out_slides });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_slides_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "slides": n }))
}

/// Split a deck into one file per slide (the presentation analogue of
/// `doc_split`/`pdf_burst`). opts: path (pptx/odp), dir => output directory,
/// format => extension override (default = source ext), prefix => file-name stem
/// (default = source stem). Each source slide's first text line becomes the title
/// and the rest the body. Returns `{ count, files: [path] }`.
fn op_slides_split(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let dir = req_str(&opts, "dir")?;
    let ext = opts
        .get("format")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| ext_of(path));
    let prefix = opts
        .get("prefix")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("slide")
                .to_string()
        });

    let read = op_slides_read(json!({ "path": path }))?;
    let slides = read
        .get("slides")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut files = Vec::new();
    for (i, s) in slides.iter().enumerate() {
        let text: Vec<String> = s
            .get("text")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let (title, body) = text
            .split_first()
            .map(|(h, rest)| (h.clone(), rest.to_vec()))
            .unwrap_or_default();
        let out = format!("{dir}/{prefix}-{}.{ext}", i + 1);
        op_slides_write(json!({
            "path": out,
            "slides": [{ "title": title, "body": body }],
            "format": ext,
        }))?;
        files.push(out);
    }
    Ok(json!({ "count": files.len(), "files": files }))
}

/// Statistics for a presentation (pptx/odp). opts: path. Returns `{ slides,
/// words (in slide text), notes_words, per_slide: [{ words, notes_words }] }`.
fn op_slides_stats(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let read = op_slides_read(json!({ "path": path }))?;
    let slides = read
        .get("slides")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let count_words = |s: &Value, field: &str| -> u64 {
        s.get(field)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|t| t.split_whitespace().count() as u64)
                    .sum()
            })
            .unwrap_or(0)
    };
    let (mut words, mut notes_words) = (0u64, 0u64);
    let mut per_slide = Vec::with_capacity(slides.len());
    for s in &slides {
        let w = count_words(s, "text");
        let nw = count_words(s, "notes");
        words += w;
        notes_words += nw;
        per_slide.push(json!({ "words": w, "notes_words": nw }));
    }
    Ok(json!({
        "slides": slides.len(),
        "words": words,
        "notes_words": notes_words,
        "per_slide": per_slide,
    }))
}

/// Append slides to an existing presentation. opts: path, slides => [{title,
/// body}], output (default: in place), format. Existing slides are read back
/// into {title, body} (first text line is the title), so the target format
/// follows the output extension. Returns `{ ok, path, slides, added }`.
fn op_slides_append(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let add = opts
        .get("slides")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing slides (expected array)"))?;

    let read = op_slides_read(json!({ "path": path }))?;
    let existing = read
        .get("slides")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out_slides: Vec<Value> = Vec::new();
    for s in &existing {
        let text: Vec<String> = s
            .get("text")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let (title, body) = text
            .split_first()
            .map(|(h, rest)| (h.clone(), rest.to_vec()))
            .unwrap_or_default();
        out_slides.push(json!({ "title": title, "body": body }));
    }
    let added = add.len();
    out_slides.extend(add.iter().cloned());
    let total = out_slides.len();

    let mut wopts = json!({ "path": output, "slides": out_slides });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_slides_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "slides": total, "added": added }))
}

// ── pdf (self-contained, via lo_core) ────────────────────────────────────────

fn op_pdf_read(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let bytes = std::fs::read(path)?;
    let pages = lo_core::extract_pages_from_pdf(&bytes).map_err(|e| anyhow!("pdf parse: {e}"))?;
    let text = lo_core::extract_text_from_pdf(&bytes).map_err(|e| anyhow!("pdf parse: {e}"))?;
    Ok(json!({"pages": pages, "text": text}))
}

fn op_pdf_write(opts: Value) -> Result<Value> {
    use lo_core::units::Length;
    let path = req_str(&opts, "path")?.to_string();
    let lines: Vec<String> = opts
        .get("lines")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing lines (expected array of strings)"))?
        .iter()
        .map(cell_to_string)
        .collect();
    let bytes = lo_core::write_text_pdf(&lines, Length::mm(210.0), Length::mm(297.0));
    std::fs::write(&path, &bytes)?;
    Ok(json!({"ok": true, "path": path, "lines": lines.len(), "bytes": bytes.len()}))
}

// ── ffi boundary ─────────────────────────────────────────────────────────────

fn ffi_call<F>(args: *const c_char, handler: F) -> *const c_char
where
    F: FnOnce(Value) -> Result<Value>,
{
    let input = if args.is_null() {
        Value::Null
    } else {
        let cs = unsafe { CStr::from_ptr(args) };
        serde_json::from_slice::<Value>(cs.to_bytes()).unwrap_or(Value::Null)
    };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| handler(input)));
    let out = match result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => json!({ "error": e.to_string() }),
        Err(_) => json!({ "error": "stryke-office handler panicked" }),
    };
    let s =
        serde_json::to_string(&out).unwrap_or_else(|_| String::from(r#"{"error":"serialize"}"#));
    match CString::new(s) {
        Ok(c) => c.into_raw() as *const c_char,
        Err(_) => std::ptr::null(),
    }
}

/// Free a C string allocated by any export from this cdylib.
///
/// # Safety
/// `p` must be a pointer previously returned by an export, or null.
#[no_mangle]
pub unsafe extern "C" fn stryke_free_cstring(p: *mut c_char) {
    if p.is_null() {
        return;
    }
    drop(CString::from_raw(p));
}

macro_rules! export {
    ($name:ident, $handler:path) => {
        #[no_mangle]
        pub extern "C" fn $name(args: *const c_char) -> *const c_char {
            ffi_call(args, $handler)
        }
    };
}

#[no_mangle]
pub extern "C" fn office__pkg_version(args: *const c_char) -> *const c_char {
    ffi_call(args, |_| Ok(json!({"version": env!("CARGO_PKG_VERSION")})))
}

export!(office__sheet_read, op_sheet_read);
export!(office__sheet_write, op_sheet_write);
export!(office__sheet_merge, op_sheet_merge);
export!(office__sheet_union, op_sheet_union);
export!(office__sheet_stats, op_sheet_stats);
export!(office__sheet_describe, op_sheet_describe);
export!(office__sheet_quantile, op_sheet_quantile);
export!(office__sheet_corr, op_sheet_corr);
export!(office__sheet_to_md, op_sheet_to_md);
export!(office__md_to_sheet, op_md_to_sheet);
export!(office__sheet_to_html, op_sheet_to_html);
export!(office__sheet_to_text, op_sheet_to_text);
export!(office__sheet_get_cell, op_sheet_get_cell);
export!(office__sheet_set_cell, op_sheet_set_cell);
export!(office__sheet_get_range, op_sheet_get_range);
export!(office__sheet_set_range, op_sheet_set_range);
export!(office__sheet_insert_rows, op_sheet_insert_rows);
export!(office__sheet_delete_rows, op_sheet_delete_rows);
export!(office__sheet_insert_column, op_sheet_insert_column);
export!(office__sheet_cumsum, op_sheet_cumsum);
export!(office__sheet_pct, op_sheet_pct);
export!(office__sheet_normalize, op_sheet_normalize);
export!(office__sheet_movavg, op_sheet_movavg);
export!(office__sheet_delta, op_sheet_delta);
export!(office__sheet_clamp, op_sheet_clamp);
export!(office__sheet_rename_column, op_sheet_rename_column);
export!(office__sheet_explode, op_sheet_explode);
export!(office__sheet_map, op_sheet_map);
export!(office__sheet_partition, op_sheet_partition);
export!(office__sheet_multisort, op_sheet_multisort);
export!(office__sheet_find, op_sheet_find);
export!(office__sheet_records, op_sheet_records);
export!(office__records_write, op_records_write);
export!(office__sheet_to_json, op_sheet_to_json);
export!(office__sheet_to_ndjson, op_sheet_to_ndjson);
export!(office__ndjson_to_sheet, op_ndjson_to_sheet);
export!(office__json_to_sheet, op_json_to_sheet);
export!(office__sheet_sort, op_sheet_sort);
export!(office__sheet_rank, op_sheet_rank);
export!(office__sheet_filter, op_sheet_filter);
export!(office__sheet_aggregate, op_sheet_aggregate);
export!(office__sheet_group_concat, op_sheet_group_concat);
export!(office__sheet_lookup, op_sheet_lookup);
export!(office__sheet_countif, op_sheet_countif);
export!(office__sheet_sumif, op_sheet_sumif);
export!(office__sheet_freq, op_sheet_freq);
export!(office__sheet_split_column, op_sheet_split_column);
export!(office__sheet_concat_columns, op_sheet_concat_columns);
export!(office__sheet_pivot, op_sheet_pivot);
export!(office__sheet_unpivot, op_sheet_unpivot);
export!(office__sheet_join, op_sheet_join);
export!(office__sheet_select, op_sheet_select);
export!(office__sheet_drop, op_sheet_drop);
export!(office__sheet_add_column, op_sheet_add_column);
export!(office__sheet_totals, op_sheet_totals);
export!(office__sheet_replace, op_sheet_replace);
export!(office__sheet_transpose, op_sheet_transpose);
export!(office__sheet_dedupe, op_sheet_dedupe);
export!(office__sheet_append, op_sheet_append);
export!(office__sheet_fill, op_sheet_fill);
export!(office__sheet_drop_empty, op_sheet_drop_empty);
export!(office__sheet_add_header, op_sheet_add_header);
export!(office__sheet_calc, op_sheet_calc);
export!(office__sheet_where, op_sheet_where);
export!(office__sheet_split, op_sheet_split);
export!(office__sheet_chunk, op_sheet_chunk);
export!(office__sheet_head, op_sheet_head);
export!(office__sheet_sample, op_sheet_sample);
export!(office__sheet_transform, op_sheet_transform);
export!(office__sheet_top, op_sheet_top);
export!(office__sheet_rename, op_sheet_rename);
export!(office__sheet_add, op_sheet_add);
export!(office__sheet_remove, op_sheet_remove);
export!(office__sheet_reorder, op_sheet_reorder);
export!(office__sheet_diff, op_sheet_diff);
export!(office__sheet_info, op_sheet_info);
export!(office__info, op_info);
export!(office__sheet_to_slides, op_sheet_to_slides);
export!(office__sheet_validate, op_sheet_validate);
export!(office__doc_read, op_doc_read);
export!(office__doc_write, op_doc_write);
export!(office__slides_read, op_slides_read);
export!(office__slides_write, op_slides_write);
export!(office__slides_add_image, op_slides_add_image);
export!(office__slides_set_notes, op_slides_set_notes);
export!(office__slides_add_text, op_slides_add_text);
export!(office__slides_merge, op_slides_merge);
export!(office__slides_reorder, op_slides_reorder);
export!(office__slides_split, op_slides_split);
export!(office__slides_stats, op_slides_stats);
export!(office__slides_append, op_slides_append);
export!(office__pdf_read, op_pdf_read);
export!(office__pdf_write, op_pdf_write);

// multi-element PDF document builder (text/images/shapes across pages)
include!("pdf_build.rs");
export!(office__pdf_build, op_pdf_build);
export!(office__images_to_pdf, op_images_to_pdf);
export!(office__sheet_to_pdf, op_sheet_to_pdf);

// PDF manipulation (merge/split/rotate/info) via lopdf
include!("pdf_ops.rs");
export!(office__pdf_merge, op_pdf_merge);
export!(office__pdf_split, op_pdf_split);
export!(office__pdf_rotate, op_pdf_rotate);
export!(office__pdf_info, op_pdf_info);
export!(office__pdf_page_sizes, op_pdf_page_sizes);
export!(office__pdf_watermark, op_pdf_watermark);
export!(office__pdf_page_numbers, op_pdf_page_numbers);
export!(office__pdf_encrypt, op_pdf_encrypt);
export!(office__pdf_decrypt, op_pdf_decrypt);
export!(office__pdf_compress, op_pdf_compress);
export!(office__pdf_delete, op_pdf_delete);
export!(office__pdf_reorder, op_pdf_reorder);
export!(office__pdf_search, op_pdf_search);
export!(office__pdf_crop, op_pdf_crop);
export!(office__pdf_burst, op_pdf_burst);
export!(office__pdf_chunk, op_pdf_chunk);
export!(office__pdf_split_ranges, op_pdf_split_ranges);
export!(office__pdf_split_bookmarks, op_pdf_split_bookmarks);
export!(office__pdf_to_text, op_pdf_to_text);
export!(office__pdf_stats, op_pdf_stats);
export!(office__pdf_assemble, op_pdf_assemble);
export!(office__pdf_stamp_image, op_pdf_stamp_image);
export!(office__pdf_insert, op_pdf_insert);
export!(office__pdf_draw_rect, op_pdf_draw_rect);
export!(office__pdf_add_text, op_pdf_add_text);
export!(office__pdf_draw_line, op_pdf_draw_line);
export!(office__pdf_add_link, op_pdf_add_link);
export!(office__pdf_links, op_pdf_links);
export!(office__pdf_remove_annotations, op_pdf_remove_annotations);
export!(office__pdf_highlight, op_pdf_highlight);
export!(office__pdf_annotations, op_pdf_annotations);

// PDF file attachments (embedded files): embed + list/extract
include!("pdf_attach.rs");
export!(office__pdf_attach, op_pdf_attach);
export!(office__pdf_attachments, op_pdf_attachments);

// PDF AcroForm fields: list + fill
include!("pdf_form.rs");
export!(office__pdf_form_fields, op_pdf_form_fields);
export!(office__pdf_fill_form, op_pdf_fill_form);

// PDF document outline (bookmarks): read + write
include!("pdf_outline.rs");
export!(office__pdf_outline, op_pdf_outline);
export!(office__pdf_set_outline, op_pdf_set_outline);

// document metadata (core/app properties) read + write across all formats
include!("meta_ops.rs");
export!(office__meta_read, op_meta_read);
export!(office__meta_write, op_meta_write);

// embedded media extraction (OOXML/ODF media parts + PDF image XObjects)
include!("extract.rs");
export!(office__extract_images, op_extract_images);

// document text search/replace (template / mail-merge filling)
include!("textops.rs");
export!(office__replace_text, op_replace_text);
export!(office__mail_merge, op_mail_merge);
export!(office__text_replace, op_text_replace);

// structured document reads (tables) — read-side mirror of doc_write blocks
include!("doc_struct.rs");
export!(office__doc_tables, op_doc_tables);
export!(office__doc_table_to_sheet, op_doc_table_to_sheet);
export!(office__sheet_to_doc, op_sheet_to_doc);
export!(office__doc_blocks, op_doc_blocks);
export!(office__doc_outline, op_doc_outline);
export!(office__doc_links, op_doc_links);
export!(office__doc_stats, op_doc_stats);
export!(office__doc_merge, op_doc_merge);
export!(office__doc_append, op_doc_append);
export!(office__doc_split, op_doc_split);
export!(office__md_to_doc, op_md_to_doc);
export!(office__doc_to_md, op_doc_to_md);
export!(office__doc_to_html, op_doc_to_html);
export!(office__doc_to_text, op_doc_to_text);
export!(office__pdf_to_doc, op_pdf_to_doc);
export!(office__pdf_to_slides, op_pdf_to_slides);
export!(office__doc_to_pdf, op_doc_to_pdf);
export!(office__html_to_pdf, op_html_to_pdf);
export!(office__md_to_pdf, op_md_to_pdf);
export!(office__doc_add_toc, op_doc_add_toc);
export!(office__doc_to_slides, op_doc_to_slides);
export!(office__slides_to_doc, op_slides_to_doc);
export!(office__slides_to_pdf, op_slides_to_pdf);
export!(office__slides_outline, op_slides_outline);
export!(office__slides_to_md, op_slides_to_md);
export!(office__slides_to_html, op_slides_to_html);
export!(office__slides_to_text, op_slides_to_text);
export!(office__slides_to_sheet, op_slides_to_sheet);
export!(office__pdf_to_sheet, op_pdf_to_sheet);
export!(office__doc_to_sheet, op_doc_to_sheet);
export!(office__md_to_slides, op_md_to_slides);
export!(office__html_to_doc, op_html_to_doc);
export!(office__html_to_sheet, op_html_to_sheet);
export!(office__doc_wordfreq, op_doc_wordfreq);
export!(office__doc_find, op_doc_find);
export!(office__slides_find, op_slides_find);

// plain-text office formats (csv/tsv, html/md/rtf/txt)
include!("doc_formats.rs");

// PIL-style image I/O + manipulation (image crate)
include!("image_ops.rs");

export!(office__img_open, op_img_open);
export!(office__img_new, op_img_new);
export!(office__img_save, op_img_save);
export!(office__img_info, op_img_info);
export!(office__img_resize, op_img_resize);
export!(office__img_thumbnail, op_img_thumbnail);
export!(office__img_crop, op_img_crop);
export!(office__img_rotate, op_img_rotate);
export!(office__img_flip, op_img_flip);
export!(office__img_convert, op_img_convert);
export!(office__img_paste, op_img_paste);
export!(office__img_get_pixel, op_img_get_pixel);
export!(office__img_put_pixel, op_img_put_pixel);
export!(office__img_draw_rect, op_img_draw_rect);
export!(office__img_draw_line, op_img_draw_line);
export!(office__img_draw_circle, op_img_draw_circle);
export!(office__img_draw_text, op_img_draw_text);
export!(office__img_close, op_img_close);
export!(office__img_blur, op_img_blur);
export!(office__img_sharpen, op_img_sharpen);
export!(office__img_brighten, op_img_brighten);
export!(office__img_contrast, op_img_contrast);
export!(office__img_huerotate, op_img_huerotate);
export!(office__img_invert, op_img_invert);
export!(office__img_grayscale, op_img_grayscale);
export!(office__img_gamma, op_img_gamma);
export!(office__img_threshold, op_img_threshold);
export!(office__img_posterize, op_img_posterize);
export!(office__img_sepia, op_img_sepia);
export!(office__img_tint, op_img_tint);

// extended PIL-complete processing surface
export!(office__img_autocontrast, op_img_autocontrast);
export!(office__img_equalize, op_img_equalize);
export!(office__img_solarize, op_img_solarize);
export!(office__img_colorize, op_img_colorize);
export!(office__img_emboss, op_img_emboss);
export!(office__img_convolve, op_img_convolve);
export!(office__img_edges, op_img_edges);
export!(office__img_box_blur, op_img_box_blur);
export!(office__img_median, op_img_median);
export!(office__img_pixelate, op_img_pixelate);
export!(office__img_vignette, op_img_vignette);
export!(office__img_opacity, op_img_opacity);
export!(office__img_putalpha, op_img_putalpha);
export!(office__img_blend, op_img_blend);
export!(office__img_blend_mode, op_img_blend_mode);
export!(office__img_composite, op_img_composite);
export!(office__img_border, op_img_border);
export!(office__img_trim, op_img_trim);
export!(office__img_transpose, op_img_transpose);
export!(office__img_transverse, op_img_transverse);
export!(office__img_histogram, op_img_histogram);
export!(office__img_extrema, op_img_extrema);
export!(office__img_noise, op_img_noise);
export!(office__img_watermark, op_img_watermark);
export!(office__img_split, op_img_split);
export!(office__img_merge, op_img_merge);
export!(office__img_dilate, op_img_dilate);
export!(office__img_erode, op_img_erode);

// animation, advanced drawing, transforms, byte I/O
export!(office__img_open_frames, op_img_open_frames);
export!(office__img_save_animated, op_img_save_animated);
export!(office__img_montage, op_img_montage);
export!(office__img_gradient, op_img_gradient);
export!(office__img_draw_ellipse, op_img_draw_ellipse);
export!(office__img_draw_polygon, op_img_draw_polygon);
export!(office__img_draw_text_multiline, op_img_draw_text_multiline);
export!(office__img_warp, op_img_warp);
export!(office__img_to_base64, op_img_to_base64);
export!(office__img_from_base64, op_img_from_base64);
export!(office__img_data_uri, op_img_data_uri);
export!(office__img_resize_file, op_img_resize_file);
export!(office__contact_sheet, op_contact_sheet);

// shapes, fills, masks, color analysis
export!(office__img_draw_rounded_rect, op_img_draw_rounded_rect);
export!(office__img_draw_polyline, op_img_draw_polyline);
export!(office__img_draw_arc, op_img_draw_arc);
export!(office__img_flood_fill, op_img_flood_fill);
export!(office__img_replace_color, op_img_replace_color);
export!(office__img_swap_channels, op_img_swap_channels);
export!(office__img_dominant_colors, op_img_dominant_colors);
export!(office__img_compare, op_img_compare);
export!(office__img_text_size, op_img_text_size);
export!(office__img_caption, op_img_caption);
export!(office__img_crop_circle, op_img_crop_circle);
export!(office__img_round_corners, op_img_round_corners);
export!(office__img_drop_shadow, op_img_drop_shadow);

// color science + distortions
export!(office__img_levels, op_img_levels);
export!(office__img_curves, op_img_curves);
export!(office__img_hsl, op_img_hsl);
export!(office__img_temperature, op_img_temperature);
export!(office__img_channel_mixer, op_img_channel_mixer);
export!(office__img_swirl, op_img_swirl);
export!(office__img_wave, op_img_wave);
export!(office__img_fisheye, op_img_fisheye);
export!(office__img_kaleidoscope, op_img_kaleidoscope);
export!(office__img_spritesheet, op_img_spritesheet);
export!(office__img_seam_carve, op_img_seam_carve);
export!(office__img_dither, op_img_dither);
export!(office__img_quantize, op_img_quantize);
export!(office__img_favicon, op_img_favicon);

// barcode + QR-code generation (-> image handle, composes with image surface)
include!("barcode.rs");
export!(office__barcode_qr, op_barcode_qr);
export!(office__barcode_1d, op_barcode_1d);
export!(office__barcode_save, op_barcode_save);

// standalone chart rendering (-> image handle, save to any format)
include!("chart_render.rs");
include!("chart_svg.rs");

export!(office__chart_render, op_chart_render);
export!(office__chart_from_sheet, op_chart_from_sheet);
export!(office__chart_svg, op_chart_svg);
export!(office__chart_save, op_chart_save);
export!(office__chart_grid, op_chart_grid);

// minimal pptx writer (OOXML via zip + hand-built XML)
include!("pptx_write.rs");

#[cfg(test)]
mod tests;
