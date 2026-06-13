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
    match ext_of(path).as_str() {
        "ods" => return read_ods(path),
        "csv" => return read_csv(path, ','),
        "tsv" => return read_csv(path, '\t'),
        _ => {}
    }
    let want_formulas = opts
        .get("formulas")
        .and_then(Value::as_bool)
        .unwrap_or(false);
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
    if o.get("bold").and_then(Value::as_bool) == Some(true) {
        f = f.set_bold();
        used = true;
    }
    if o.get("italic").and_then(Value::as_bool) == Some(true) {
        f = f.set_italic();
        used = true;
    }
    if o.get("underline").and_then(Value::as_bool) == Some(true) {
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
    if o.get("border").and_then(Value::as_bool) == Some(true) {
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
    if s.get("protect").and_then(Value::as_bool) == Some(true) {
        ws.protect();
    }
    if let Some(c) = s.get("tab_color").and_then(Value::as_str) {
        ws.set_tab_color(xlsx_color(c));
    }
    if let Some(z) = s.get("zoom").and_then(Value::as_u64) {
        ws.set_zoom(z as u16);
    }
    if s.get("landscape").and_then(Value::as_bool) == Some(true) {
        ws.set_landscape();
    }
    if let Some(p) = s.get("paper").and_then(Value::as_u64) {
        ws.set_paper_size(p as u8);
    }
    if s.get("print_gridlines").and_then(Value::as_bool) == Some(true) {
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
    if s.get("autofit").and_then(Value::as_bool) == Some(true) {
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
        if sp.get("markers").and_then(Value::as_bool) == Some(true) {
            spark = spark.show_markers(true);
        }
        if sp.get("high").and_then(Value::as_bool) == Some(true) {
            spark = spark.show_high_point(true);
        }
        if sp.get("low").and_then(Value::as_bool) == Some(true) {
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
            let delim = if target_ext(&opts, &path) == "tsv" {
                '\t'
            } else {
                ','
            };
            write_csv(&path, &sheets, delim)?;
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
    let header = opts.get("header").and_then(Value::as_bool).unwrap_or(true);
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
    let ignore_case = opts
        .get("ignore_case")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let whole = opts.get("whole").and_then(Value::as_bool).unwrap_or(false);
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

    let header = opts.get("header").and_then(Value::as_bool).unwrap_or(true);
    let descending = opts
        .get("descending")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ignore_case = opts
        .get("ignore_case")
        .and_then(Value::as_bool)
        .unwrap_or(false);

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
    let numeric = match opts.get("numeric").and_then(Value::as_bool) {
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
    let ignore_case = opts
        .get("ignore_case")
        .and_then(Value::as_bool)
        .unwrap_or(false);

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

    let header = opts.get("header").and_then(Value::as_bool).unwrap_or(true);
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
    let header = opts.get("header").and_then(Value::as_bool).unwrap_or(true);
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

    let header = opts.get("header").and_then(Value::as_bool).unwrap_or(true);
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

    let header = opts.get("header").and_then(Value::as_bool).unwrap_or(true);
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
    } else {
        b.get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }
}

/// Build a formatted docx Run from `{text, bold, italic, underline, strike,
/// size (pt), color, font, highlight}`.
fn docx_run(j: &Value) -> docx_rs::Run {
    use docx_rs::{Run, RunFonts};
    let mut run = Run::new().add_text(j.get("text").and_then(Value::as_str).unwrap_or(""));
    if j.get("bold").and_then(Value::as_bool) == Some(true) {
        run = run.bold();
    }
    if j.get("italic").and_then(Value::as_bool) == Some(true) {
        run = run.italic();
    }
    if j.get("strike").and_then(Value::as_bool) == Some(true) {
        run = run.strike();
    }
    if j.get("underline").and_then(Value::as_bool) == Some(true) {
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
        Level, LevelJc, LevelText, NumberFormat, Numbering, NumberingId, Paragraph, Pic, Run,
        Shading, Start, Table, TableCell, TableRow, VAlignType, WidthType,
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
    if let Some(f) = opts.get("footer").and_then(Value::as_str) {
        docx = docx
            .footer(Footer::new().add_paragraph(Paragraph::new().add_run(Run::new().add_text(f))));
    }
    for b in blocks {
        match b.get("kind").and_then(Value::as_str).unwrap_or("para") {
            "list" => {
                let ordered = b.get("ordered").and_then(Value::as_bool).unwrap_or(false);
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
export!(office__sheet_stats, op_sheet_stats);
export!(office__sheet_find, op_sheet_find);
export!(office__sheet_records, op_sheet_records);
export!(office__records_write, op_records_write);
export!(office__sheet_sort, op_sheet_sort);
export!(office__sheet_filter, op_sheet_filter);
export!(office__sheet_aggregate, op_sheet_aggregate);
export!(office__sheet_select, op_sheet_select);
export!(office__sheet_transpose, op_sheet_transpose);
export!(office__sheet_dedupe, op_sheet_dedupe);
export!(office__doc_read, op_doc_read);
export!(office__doc_write, op_doc_write);
export!(office__slides_read, op_slides_read);
export!(office__slides_write, op_slides_write);
export!(office__slides_merge, op_slides_merge);
export!(office__pdf_read, op_pdf_read);
export!(office__pdf_write, op_pdf_write);

// multi-element PDF document builder (text/images/shapes across pages)
include!("pdf_build.rs");
export!(office__pdf_build, op_pdf_build);

// PDF manipulation (merge/split/rotate/info) via lopdf
include!("pdf_ops.rs");
export!(office__pdf_merge, op_pdf_merge);
export!(office__pdf_split, op_pdf_split);
export!(office__pdf_rotate, op_pdf_rotate);
export!(office__pdf_info, op_pdf_info);
export!(office__pdf_watermark, op_pdf_watermark);
export!(office__pdf_page_numbers, op_pdf_page_numbers);
export!(office__pdf_encrypt, op_pdf_encrypt);
export!(office__pdf_decrypt, op_pdf_decrypt);
export!(office__pdf_compress, op_pdf_compress);
export!(office__pdf_delete, op_pdf_delete);
export!(office__pdf_reorder, op_pdf_reorder);
export!(office__pdf_search, op_pdf_search);
export!(office__pdf_burst, op_pdf_burst);

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

// structured document reads (tables) — read-side mirror of doc_write blocks
include!("doc_struct.rs");
export!(office__doc_tables, op_doc_tables);
export!(office__doc_blocks, op_doc_blocks);
export!(office__doc_links, op_doc_links);
export!(office__doc_stats, op_doc_stats);
export!(office__doc_merge, op_doc_merge);
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

// standalone chart rendering (-> image handle, save to any format)
include!("chart_render.rs");
include!("chart_svg.rs");

export!(office__chart_render, op_chart_render);
export!(office__chart_svg, op_chart_svg);
export!(office__chart_save, op_chart_save);
export!(office__chart_grid, op_chart_grid);

// minimal pptx writer (OOXML via zip + hand-built XML)
include!("pptx_write.rs");

#[cfg(test)]
mod tests;
