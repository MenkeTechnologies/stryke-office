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
    if ext_of(path) == "ods" {
        return read_ods(path);
    }
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
        sheets.push(json!({"name": name, "rows": rows}));
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

fn write_xlsx(path: &str, sheets: &[(String, Vec<Vec<Value>>)]) -> Result<()> {
    use rust_xlsxwriter::Workbook;
    let mut wb = Workbook::new();
    for (name, rows) in sheets {
        let ws = wb.add_worksheet();
        ws.set_name(name)?;
        for (r, row) in rows.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                let (r, c) = (r as u32, c as u16);
                match cell {
                    Value::Null => {}
                    Value::Bool(b) => {
                        ws.write_boolean(r, c, *b)?;
                    }
                    Value::Number(n) => {
                        ws.write_number(r, c, n.as_f64().unwrap_or(0.0))?;
                    }
                    other => {
                        ws.write_string(r, c, cell_to_string(other))?;
                    }
                }
            }
        }
    }
    wb.save(path)?;
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
    let sheets = json_sheets(&opts)?;
    match target_ext(&opts, &path).as_str() {
        "xlsx" => write_xlsx(&path, &sheets)?,
        "ods" => write_ods(&path, &sheets)?,
        other => return Err(anyhow!("unsupported spreadsheet write format: {other}")),
    }
    Ok(json!({"ok": true, "path": path, "sheets": sheets.len()}))
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
        other => return Err(anyhow!("unsupported document read format: {other}")),
    };
    Ok(json!({ "paragraphs": paragraphs }))
}

/// A doc block: {kind: "para"|"heading", level?: u8, text: string}.
fn json_blocks(opts: &Value) -> Result<Vec<(String, u8, String)>> {
    let arr = opts
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing blocks (expected array)"))?;
    Ok(arr
        .iter()
        .map(|b| {
            let kind = b
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("para")
                .to_string();
            let level = b.get("level").and_then(Value::as_u64).unwrap_or(1) as u8;
            let text = b
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (kind, level, text)
        })
        .collect())
}

fn write_docx(path: &str, blocks: &[(String, u8, String)]) -> Result<()> {
    use docx_rs::{Docx, Paragraph, Run};
    let mut docx = Docx::new();
    for (kind, level, text) in blocks {
        let mut p = Paragraph::new().add_run(Run::new().add_text(text));
        if kind == "heading" {
            p = p.style(&format!("Heading{}", (*level).clamp(1, 9)));
        }
        docx = docx.add_paragraph(p);
    }
    let file = std::fs::File::create(path)?;
    docx.build().pack(file)?;
    Ok(())
}

fn write_odt(path: &str, blocks: &[(String, u8, String)]) -> Result<()> {
    use lo_core::TextDocument;
    let mut doc = TextDocument::new("stryke-office");
    for (kind, level, text) in blocks {
        if kind == "heading" {
            doc.push_heading(*level, text.clone());
        } else {
            doc.push_paragraph(text.clone());
        }
    }
    lo_odf::save_text_document(path, &doc)?;
    Ok(())
}

fn op_doc_write(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?.to_string();
    let blocks = json_blocks(&opts)?;
    match target_ext(&opts, &path).as_str() {
        "docx" => write_docx(&path, &blocks)?,
        "odt" => write_odt(&path, &blocks)?,
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
                slides.push(json!({ "text": text }));
            }
        }
        "odp" => {
            let xml = read_zip_entry(&bytes, "content.xml")?;
            for page in split_draw_pages(&xml) {
                let text = extract_paragraphs(&page, &["text:p"]);
                slides.push(json!({ "text": text }));
            }
        }
        other => return Err(anyhow!("unsupported presentation read format: {other}")),
    }
    Ok(json!({ "slides": slides }))
}

/// A slide spec: {title?: string, body?: [string]}.
fn json_slides(opts: &Value) -> Result<Vec<(String, Vec<String>)>> {
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
                .map(|a| a.iter().map(cell_to_string).collect())
                .unwrap_or_default();
            (title, body)
        })
        .collect())
}

fn write_odp(path: &str, slides: &[(String, Vec<String>)]) -> Result<()> {
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
            text.push_str(&body.join("\n"));
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
export!(office__doc_read, op_doc_read);
export!(office__doc_write, op_doc_write);
export!(office__slides_read, op_slides_read);
export!(office__slides_write, op_slides_write);
export!(office__pdf_read, op_pdf_read);
export!(office__pdf_write, op_pdf_write);

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

// minimal pptx writer (OOXML via zip + hand-built XML)
include!("pptx_write.rs");

#[cfg(test)]
mod tests;
