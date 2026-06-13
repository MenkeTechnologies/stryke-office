//! FFI-contract + round-trip tests for stryke-office.
//!
//! Each writer is exercised end to end against its matching reader over a
//! real temp file, so a passing test means the bytes on disk actually parse
//! back. No external tools are invoked.

use super::*;

fn call(f: extern "C" fn(*const c_char) -> *const c_char, arg: &str) -> Value {
    let cs = CString::new(arg).expect("arg has no NUL");
    let raw = f(cs.as_ptr());
    assert!(!raw.is_null(), "export returned null");
    let out = unsafe { CStr::from_ptr(raw) }
        .to_str()
        .expect("utf-8")
        .to_string();
    unsafe { stryke_free_cstring(raw as *mut c_char) };
    serde_json::from_str(&out).expect("valid json")
}

fn tmp(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("stryke-office-test-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

fn err_of(v: &Value) -> &str {
    v.get("error").and_then(Value::as_str).unwrap_or("")
}

#[test]
fn pkg_version_round_trips() {
    let v = call(office__pkg_version, "{}");
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn xlsx_write_then_read_round_trips() {
    let path = tmp("rt.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"Data","rows":[["name","qty"],["widget",3],["gadget",7]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write failed: {w}");
    let r = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    let rows = &r["sheets"][0]["rows"];
    assert_eq!(rows[0][0], "name", "header cell preserved");
    assert_eq!(rows[1][0], "widget");
    assert_eq!(rows[1][1], 3.0, "numeric cell preserved as number");
    std::fs::remove_file(&path).ok();
}

#[test]
fn ods_write_then_read_round_trips() {
    let path = tmp("rt.ods");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[["a","b"],["c","d"]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "ods write failed: {w}");
    let r = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["sheets"][0]["rows"][0][0], "a");
    assert_eq!(r["sheets"][0]["rows"][1][1], "d");
    std::fs::remove_file(&path).ok();
}

#[test]
fn docx_write_then_read_round_trips() {
    let path = tmp("rt.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{path}","blocks":[{{"kind":"heading","level":1,"text":"Title"}},{{"kind":"para","text":"Hello world"}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "docx write failed: {w}");
    let r = call(office__doc_read, &format!(r#"{{"path":"{path}"}}"#));
    let paras = r["paragraphs"].as_array().expect("paragraphs array");
    let joined = paras
        .iter()
        .map(|p| p.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("|");
    assert!(joined.contains("Title"), "title text present: {joined}");
    assert!(
        joined.contains("Hello world"),
        "body text present: {joined}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn odt_write_then_read_round_trips() {
    let path = tmp("rt.odt");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{path}","blocks":[{{"kind":"heading","level":1,"text":"Chapter"}},{{"kind":"para","text":"body text here"}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "odt write failed: {w}");
    let r = call(office__doc_read, &format!(r#"{{"path":"{path}"}}"#));
    let joined = r["paragraphs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("|");
    assert!(joined.contains("Chapter"), "heading present: {joined}");
    assert!(joined.contains("body text here"), "para present: {joined}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn pptx_write_then_read_round_trips() {
    let path = tmp("rt.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{path}","slides":[{{"title":"Slide One","body":["bullet a","bullet b"]}},{{"title":"Slide Two","body":["more"]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "pptx write failed: {w}");
    assert_eq!(w["slides"], 2);
    let r = call(office__slides_read, &format!(r#"{{"path":"{path}"}}"#));
    let slides = r["slides"].as_array().expect("slides array");
    assert_eq!(slides.len(), 2, "two slides round-tripped");
    let s0 = slides[0]["text"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("|");
    assert!(s0.contains("Slide One"), "slide 1 title: {s0}");
    assert!(s0.contains("bullet a"), "slide 1 body: {s0}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn odp_write_then_read_round_trips() {
    let path = tmp("rt.odp");
    let w = call(
        office__slides_write,
        &format!(r#"{{"path":"{path}","slides":[{{"title":"Intro","body":["point one"]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "odp write failed: {w}");
    let r = call(office__slides_read, &format!(r#"{{"path":"{path}"}}"#));
    let joined = r["slides"][0]["text"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(joined.contains("Intro"), "odp slide text: {joined}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn pdf_write_then_read_round_trips() {
    let path = tmp("rt.pdf");
    let w = call(
        office__pdf_write,
        &format!(r#"{{"path":"{path}","lines":["First line","Second line"]}}"#),
    );
    assert_eq!(w["ok"], true, "pdf write failed: {w}");
    assert!(w["bytes"].as_u64().unwrap_or(0) > 0, "pdf has bytes");
    let r = call(office__pdf_read, &format!(r#"{{"path":"{path}"}}"#));
    let text = r["text"].as_str().unwrap_or("");
    assert!(text.contains("First line"), "pdf text extracted: {text}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn csv_and_tsv_round_trip() {
    for (ext, _d) in [("csv", ','), ("tsv", '\t')] {
        let path = tmp(&format!("rt.{ext}"));
        let w = call(
            office__sheet_write,
            &format!(
                r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[["name","qty"],["wid, get",3],["g\"o",7]]}}]}}"#
            ),
        );
        assert_eq!(w["ok"], true, "{ext} write failed: {w}");
        let r = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
        assert_eq!(r["sheets"][0]["rows"][0][0], "name", "{ext} header");
        assert_eq!(
            r["sheets"][0]["rows"][1][0], "wid, get",
            "{ext} quoted-comma field"
        );
        assert_eq!(r["sheets"][0]["rows"][1][1], 3.0, "{ext} numeric field");
        assert_eq!(
            r["sheets"][0]["rows"][2][0], "g\"o",
            "{ext} quoted-quote field"
        );
        std::fs::remove_file(&path).ok();
    }
}

#[test]
fn html_md_rtf_txt_doc_round_trip() {
    for ext in ["html", "md", "rtf", "txt"] {
        let path = tmp(&format!("rt.{ext}"));
        let w = call(
            office__doc_write,
            &format!(
                r#"{{"path":"{path}","blocks":[{{"kind":"heading","level":1,"text":"Title"}},{{"kind":"para","runs":[{{"text":"bold ","bold":true}},{{"text":"rest"}}]}}]}}"#
            ),
        );
        assert_eq!(w["ok"], true, "{ext} write failed: {w}");
        let r = call(office__doc_read, &format!(r#"{{"path":"{path}"}}"#));
        let joined = r["paragraphs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("|");
        assert!(joined.contains("Title"), "{ext}: title present: {joined}");
        assert!(
            joined.contains("bold") && joined.contains("rest"),
            "{ext}: runs present: {joined}"
        );
        std::fs::remove_file(&path).ok();
    }
}

#[test]
fn doc_write_to_pdf() {
    let path = tmp("doc.pdf");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{path}","blocks":[{{"kind":"heading","level":1,"text":"Report Title"}},{{"kind":"para","text":"Some body text."}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "doc->pdf write failed: {w}");
    let r = call(office__pdf_read, &format!(r#"{{"path":"{path}"}}"#));
    assert!(
        r["text"].as_str().unwrap_or("").contains("Report Title"),
        "pdf text extracted"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn missing_path_errors_cleanly() {
    let v = call(office__sheet_read, "{}");
    assert_eq!(err_of(&v), "missing path");
}

#[test]
fn unsupported_write_format_errors() {
    let v = call(
        office__sheet_write,
        r#"{"path":"/tmp/x.foo","sheets":[{"name":"S","rows":[]}]}"#,
    );
    assert!(
        err_of(&v).starts_with("unsupported spreadsheet write format"),
        "got: {}",
        err_of(&v)
    );
}

#[test]
fn malformed_json_does_not_panic() {
    let v = call(office__sheet_read, "not-json-{[}");
    assert_eq!(err_of(&v), "missing path");
}

// ── image (PIL surface) ──────────────────────────────────────────────

#[test]
fn image_new_draw_save_reopen_round_trips() {
    let path = tmp("img.png");
    // New 64x48 red canvas.
    let n = call(
        office__img_new,
        r#"{"width":64,"height":48,"color":[255,0,0,255]}"#,
    );
    let h = n["handle"].as_u64().expect("handle");
    assert_eq!(n["width"], 64);
    assert_eq!(n["mode"], "RGBA");

    // Draw a filled blue rectangle, then save as PNG.
    call(
        office__img_draw_rect,
        &format!(r#"{{"handle":{h},"x":4,"y":4,"width":20,"height":10,"color":[0,0,255,255]}}"#),
    );
    let s = call(
        office__img_save,
        &format!(r#"{{"handle":{h},"path":"{path}"}}"#),
    );
    assert_eq!(s["ok"], true, "save failed: {s}");

    // Reopen and verify dimensions + a drawn pixel.
    let o = call(office__img_open, &format!(r#"{{"path":"{path}"}}"#));
    let h2 = o["handle"].as_u64().unwrap();
    assert_eq!(o["width"], 64);
    assert_eq!(o["height"], 48);
    let px = call(
        office__img_get_pixel,
        &format!(r#"{{"handle":{h2},"x":10,"y":8}}"#),
    );
    assert_eq!(px["b"], 255, "drawn blue pixel present");
    assert_eq!(px["r"], 0);
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    call(office__img_close, &format!(r#"{{"handle":{h2}}}"#));
    std::fs::remove_file(&path).ok();
}

#[test]
fn image_resize_crop_convert() {
    let n = call(
        office__img_new,
        r#"{"width":100,"height":80,"color":[10,20,30,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    let r = call(
        office__img_resize,
        &format!(r#"{{"handle":{h},"width":50,"height":40}}"#),
    );
    assert_eq!(r["width"], 50);
    assert_eq!(r["height"], 40);
    let c = call(
        office__img_crop,
        &format!(r#"{{"handle":{h},"x":0,"y":0,"width":20,"height":20}}"#),
    );
    assert_eq!(c["width"], 20);
    let g = call(
        office__img_convert,
        &format!(r#"{{"handle":{h},"mode":"L"}}"#),
    );
    assert_eq!(g["mode"], "L", "converted to grayscale");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
}

#[test]
fn image_cross_format_png_to_jpeg() {
    let png = tmp("x.png");
    let jpg = tmp("x.jpg");
    let n = call(
        office__img_new,
        r#"{"width":32,"height":32,"color":[200,100,50,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    call(
        office__img_save,
        &format!(r#"{{"handle":{h},"path":"{png}"}}"#),
    );
    // Re-save the same handle as JPEG (format inferred from extension).
    let s = call(
        office__img_save,
        &format!(r#"{{"handle":{h},"path":"{jpg}"}}"#),
    );
    assert_eq!(s["ok"], true);
    let o = call(office__img_open, &format!(r#"{{"path":"{jpg}"}}"#));
    assert_eq!(o["width"], 32, "jpeg reopened at right size");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    call(
        office__img_close,
        &format!(r#"{{"handle":{}}}"#, o["handle"].as_u64().unwrap()),
    );
    std::fs::remove_file(&png).ok();
    std::fs::remove_file(&jpg).ok();
}

#[test]
fn image_draw_text_with_vendored_font() {
    let path = tmp("text.png");
    let n = call(
        office__img_new,
        r#"{"width":200,"height":60,"color":[255,255,255,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    let d = call(
        office__img_draw_text,
        &format!(r#"{{"handle":{h},"x":10,"y":10,"text":"Hello","size":32,"color":[0,0,0,255]}}"#),
    );
    assert_eq!(d["ok"], true, "draw_text (vendored font) succeeded: {d}");
    call(
        office__img_save,
        &format!(r#"{{"handle":{h},"path":"{path}"}}"#),
    );
    assert!(std::fs::metadata(&path).is_ok(), "text image written");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    std::fs::remove_file(&path).ok();
}

#[test]
fn xlsx_rich_cells_and_formula_round_trip() {
    let path = tmp("rich.xlsx");
    // Rich cell objects: styled text, a number with format, and a formula.
    let w = call(
        office__sheet_write,
        &format!(
            r##"{{"path":"{path}","sheets":[{{"name":"R","rows":[
                [{{"v":"Header","bold":true,"color":"#FF0000","bg":"#FFFF00","align":"center"}}],
                [{{"v":42,"num_format":"0.00","italic":true}}],
                [{{"f":"=1+2"}}]
            ]}}]}}"##
        ),
    );
    assert_eq!(w["ok"], true, "rich xlsx write failed: {w}");
    // Values must survive (formatting is write-only; calamine returns values).
    let r = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(
        r["sheets"][0]["rows"][0][0], "Header",
        "styled text value preserved"
    );
    assert_eq!(
        r["sheets"][0]["rows"][1][0], 42.0,
        "formatted number preserved"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn docx_rich_runs_and_align_round_trip() {
    let path = tmp("rich.docx");
    let w = call(
        office__doc_write,
        &format!(
            r##"{{"path":"{path}","blocks":[
                {{"kind":"para","align":"center","runs":[
                    {{"text":"Bold","bold":true,"color":"#0000FF","size":18}},
                    {{"text":" and italic","italic":true}}
                ]}}
            ]}}"##
        ),
    );
    assert_eq!(w["ok"], true, "rich docx write failed: {w}");
    let r = call(office__doc_read, &format!(r#"{{"path":"{path}"}}"#));
    let joined = r["paragraphs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("");
    assert!(
        joined.contains("Bold and italic"),
        "rich runs concatenate: {joined}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn xlsx_structure_merge_cols_freeze_hyperlink() {
    let path = tmp("struct.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{
                "name":"S",
                "rows":[["Title","x"],[{{"link":"https://example.com","v":"site"}},"y"]],
                "merges":[[0,0,0,1]],
                "cols":[{{"col":0,"width":24}}],
                "row_heights":[{{"row":0,"height":30}}],
                "freeze":[1,0],
                "autofilter":[1,0,1,1]
            }}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "structured xlsx write failed: {w}");
    let r = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(
        r["sheets"][0]["rows"][0][0], "Title",
        "merged top-left value preserved"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn xlsx_worksheet_table() {
    let path = tmp("table.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"T","rows":[["h1","h2"],["a","b"],["c","d"]],"table":[0,0,2,1]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "worksheet-table write failed: {w}");
    let r = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["sheets"][0]["rows"][1][0], "a", "table data preserved");
    std::fs::remove_file(&path).ok();
}

#[test]
fn xlsx_chart_writes_valid_file() {
    let path = tmp("chart.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{
                "name":"D",
                "rows":[["Q","Sales"],["Q1",100],["Q2",150],["Q3",120]],
                "charts":[{{"type":"column","at":[0,3],"title":"Sales",
                    "series":[{{"name":"Sales","categories":[1,0,3,0],"values":[1,1,3,1]}}]}}]
            }}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "chart write failed: {w}");
    // File must be valid + data intact (calamine ignores the chart object).
    let r = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(
        r["sheets"][0]["rows"][1][1], 100.0,
        "chart-sheet data preserved"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn docx_table_and_pagebreak_round_trip() {
    let path = tmp("table.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{path}","blocks":[
                {{"kind":"table","rows":[["Name","Qty"],["Widget","3"]]}},
                {{"kind":"pagebreak"}},
                {{"kind":"para","text":"After the break"}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "docx table+pagebreak write failed: {w}");
    let r = call(office__doc_read, &format!(r#"{{"path":"{path}"}}"#));
    let joined = r["paragraphs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        joined.contains("Widget"),
        "table cell text present: {joined}"
    );
    assert!(
        joined.contains("After the break"),
        "post-break para present: {joined}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn pptx_rich_body_runs_round_trip() {
    let path = tmp("rich.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r##"{{"path":"{path}","slides":[{{"title":"Deck","body":[
                {{"text":"Big red point","bold":true,"size":24,"color":"#FF0000"}},
                "plain bullet"
            ]}}]}}"##
        ),
    );
    assert_eq!(w["ok"], true, "rich pptx write failed: {w}");
    let r = call(office__slides_read, &format!(r#"{{"path":"{path}"}}"#));
    let joined = r["slides"][0]["text"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        joined.contains("Big red point"),
        "styled run text present: {joined}"
    );
    assert!(
        joined.contains("plain bullet"),
        "plain run present: {joined}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn docx_inline_image() {
    let png = tmp("inline.png");
    let docx = tmp("withimg.docx");
    // Make a small PNG to embed.
    let n = call(
        office__img_new,
        r#"{"width":40,"height":40,"color":[0,128,0,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    call(
        office__img_save,
        &format!(r#"{{"handle":{h},"path":"{png}"}}"#),
    );
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{docx}","blocks":[
                {{"kind":"para","text":"Logo:"}},
                {{"kind":"image","path":"{png}","width":40,"height":40}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "docx inline image write failed: {w}");
    // Reopen: must parse without error and keep the caption paragraph.
    let r = call(office__doc_read, &format!(r#"{{"path":"{docx}"}}"#));
    let joined = r["paragraphs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        joined.contains("Logo:"),
        "caption preserved alongside image: {joined}"
    );
    std::fs::remove_file(&png).ok();
    std::fs::remove_file(&docx).ok();
}

// ── charting (data -> image handle -> any format) ────────────────────

#[test]
fn chart_render_to_image_then_save_any_format() {
    for kind in ["bar", "line", "area", "scatter", "pie"] {
        let series = if kind == "scatter" {
            r#"[{"name":"pts","data":[[1,2],[2,5],[3,3]]}]"#
        } else {
            r#"[{"name":"Sales","data":[10,25,15,30]},{"name":"Cost","data":[8,12,9,20]}]"#
        };
        let c = call(
            office__chart_render,
            &format!(
                r#"{{"type":"{kind}","title":"Demo","width":640,"height":400,"categories":["Q1","Q2","Q3","Q4"],"series":{series}}}"#
            ),
        );
        let h = c["handle"]
            .as_u64()
            .unwrap_or_else(|| panic!("{kind}: no handle: {c}"));
        assert_eq!(c["width"], 640, "{kind}: chart width");
        // The chart is a normal image handle — save to any format, reopen.
        let png = tmp(&format!("chart-{kind}.png"));
        let s = call(
            office__img_save,
            &format!(r#"{{"handle":{h},"path":"{png}"}}"#),
        );
        assert_eq!(s["ok"], true, "{kind}: save png failed: {s}");
        let jpg = tmp(&format!("chart-{kind}.jpg"));
        call(
            office__img_save,
            &format!(r#"{{"handle":{h},"path":"{jpg}"}}"#),
        );
        let o = call(office__img_open, &format!(r#"{{"path":"{png}"}}"#));
        assert_eq!(o["width"], 640, "{kind}: reopened chart width");
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
        call(
            office__img_close,
            &format!(r#"{{"handle":{}}}"#, o["handle"].as_u64().unwrap()),
        );
        std::fs::remove_file(&png).ok();
        std::fs::remove_file(&jpg).ok();
    }
}

#[test]
fn image_filters_apply() {
    let n = call(
        office__img_new,
        r#"{"width":48,"height":48,"color":[120,160,200,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    for (f, arg) in [
        (
            office__img_blur as extern "C" fn(*const c_char) -> *const c_char,
            r#"{"handle":H,"sigma":1.5}"#,
        ),
        (
            office__img_sharpen,
            r#"{"handle":H,"sigma":1.0,"threshold":2}"#,
        ),
        (office__img_brighten, r#"{"handle":H,"value":20}"#),
        (office__img_contrast, r#"{"handle":H,"value":15.0}"#),
        (office__img_huerotate, r#"{"handle":H,"degrees":90}"#),
        (office__img_invert, r#"{"handle":H}"#),
        (office__img_grayscale, r#"{"handle":H}"#),
    ] {
        let v = call(f, &arg.replace('H', &h.to_string()));
        assert_eq!(v["ok"], true, "filter failed: {v}");
    }
    // Image is still valid + usable after the filter chain.
    let info = call(office__img_info, &format!(r#"{{"handle":{h}}}"#));
    assert_eq!(info["width"], 48, "image intact after filters");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
}

#[test]
fn chart_extra_types_render() {
    for kind in ["donut", "stacked", "histogram", "column"] {
        let c = call(
            office__chart_render,
            &format!(
                r#"{{"type":"{kind}","width":400,"height":300,"categories":["a","b","c","d"],"series":[{{"name":"s1","data":[4,8,2,6]}},{{"name":"s2","data":[3,5,7,1]}}]}}"#
            ),
        );
        let h = c["handle"]
            .as_u64()
            .unwrap_or_else(|| panic!("{kind}: {c}"));
        assert_eq!(c["width"], 400, "{kind} width");
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    }
}

#[test]
fn chart_svg_vector_output() {
    for kind in ["bar", "line", "pie", "scatter", "donut"] {
        let series = if kind == "scatter" {
            r#"[{"data":[[1,2],[3,4]]}]"#
        } else {
            r#"[{"name":"s","data":[5,9,3]}]"#
        };
        let v = call(
            office__chart_svg,
            &format!(r#"{{"type":"{kind}","series":{series},"categories":["a","b","c"]}}"#),
        );
        let svg = v["svg"].as_str().unwrap_or("");
        assert!(
            svg.starts_with("<svg"),
            "{kind}: not svg: {}",
            &svg[..svg.len().min(40)]
        );
        assert!(svg.ends_with("</svg>"), "{kind}: unterminated svg");
    }
}

#[test]
fn pdf_build_multi_element_document() {
    // render a chart to an image handle to embed in the PDF
    let chart = call(
        office__chart_render,
        r#"{"type":"bar","width":300,"height":200,"categories":["a","b"],"series":[{"data":[3,7]}]}"#,
    );
    let ch = chart["handle"].as_u64().expect("chart handle");

    let path = tmp("doc.pdf");
    let long = "Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua ".repeat(8);
    let v = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{path}","elements":[
                {{"type":"heading","level":1,"text":"Quarterly Report"}},
                {{"type":"paragraph","text":"{long}"}},
                {{"type":"rect","x":50,"y":120,"width":200,"height":30,"color":[210,225,245]}},
                {{"type":"image","handle":{ch},"width":260,"height":173}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":2,"text":"Appendix"}},
                {{"type":"line","x0":50,"y0":120,"x1":300,"y1":120,"color":[150,150,150]}},
                {{"type":"text","x":50,"y":140,"text":"footnote","size":9}}
            ]}}"#
        ),
    );
    assert_eq!(v["ok"], true, "pdf_build failed: {v}");
    assert!(
        v["pages"].as_u64().unwrap() >= 2,
        "auto-paginated to >=2 pages: {v}"
    );
    // re-read the produced PDF with the existing extractor → valid, has text
    let r = call(office__pdf_read, &format!(r#"{{"path":"{path}"}}"#));
    let text = r["text"].as_str().unwrap_or("");
    assert!(
        text.contains("Quarterly Report") || text.contains("Appendix"),
        "pdf text round-trips: {:?}",
        &text[..text.len().min(80)]
    );
    call(office__img_close, &format!(r#"{{"handle":{ch}}}"#));
    std::fs::remove_file(&path).ok();
}

#[test]
fn pdf_merge_split_rotate_info() {
    // make two 2-page source PDFs via pdf_build
    let a = tmp("a.pdf");
    let b = tmp("b.pdf");
    for (path, head) in [(&a, "Alpha"), (&b, "Beta")] {
        let v = call(
            office__pdf_build,
            &format!(
                r#"{{"path":"{path}","elements":[
                    {{"type":"heading","level":1,"text":"{head} 1"}},
                    {{"type":"pagebreak"}},
                    {{"type":"heading","level":1,"text":"{head} 2"}}
                ]}}"#
            ),
        );
        assert_eq!(v["ok"], true, "build {head}: {v}");
    }
    // info on a source
    let ia = call(office__pdf_info, &format!(r#"{{"path":"{a}"}}"#));
    assert_eq!(ia["pages"], 2, "source A has 2 pages: {ia}");

    // merge → 4 pages
    let merged = tmp("merged.pdf");
    let m = call(
        office__pdf_merge,
        &format!(r#"{{"inputs":["{a}","{b}"],"path":"{merged}"}}"#),
    );
    assert_eq!(m["ok"], true, "merge: {m}");
    let im = call(office__pdf_info, &format!(r#"{{"path":"{merged}"}}"#));
    assert_eq!(im["pages"], 4, "merged has 4 pages: {im}");

    // split → keep pages 1 and 3 → 2 pages
    let sub = tmp("sub.pdf");
    let s = call(
        office__pdf_split,
        &format!(r#"{{"path":"{merged}","pages":[1,3],"output":"{sub}"}}"#),
    );
    assert_eq!(s["ok"], true, "split: {s}");
    let is = call(office__pdf_info, &format!(r#"{{"path":"{sub}"}}"#));
    assert_eq!(is["pages"], 2, "split kept 2 pages: {is}");

    // rotate all pages 90°
    let rot = tmp("rot.pdf");
    let r = call(
        office__pdf_rotate,
        &format!(r#"{{"path":"{merged}","angle":90,"output":"{rot}"}}"#),
    );
    assert_eq!(r["rotated"], 4, "rotated all 4 pages: {r}");
    // rotated file still parses
    let ir = call(office__pdf_read, &format!(r#"{{"path":"{rot}"}}"#));
    assert!(ir["text"].is_string(), "rotated pdf re-reads");

    for f in [&a, &b, &merged, &sub, &rot] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_watermark_and_page_numbers() {
    // 3-page source
    let src = tmp("wm_src.pdf");
    let v = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[
                {{"type":"heading","level":1,"text":"One"}},{{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Two"}},{{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Three"}}
            ]}}"#
        ),
    );
    assert_eq!(v["ok"], true, "build src: {v}");

    // watermark every page
    let wm = tmp("wm.pdf");
    let w = call(
        office__pdf_watermark,
        &format!(r#"{{"path":"{src}","output":"{wm}","text":"DRAFT","angle":45}}"#),
    );
    assert_eq!(w["stamped"], 3, "watermarked 3 pages: {w}");
    let ir = call(office__pdf_read, &format!(r#"{{"path":"{wm}"}}"#));
    assert!(
        ir["text"].as_str().unwrap_or("").contains("One"),
        "watermarked pdf keeps content + re-reads"
    );

    // footer page numbers
    let pn = tmp("pn.pdf");
    let p = call(
        office__pdf_page_numbers,
        &format!(r#"{{"path":"{src}","output":"{pn}","format":"Page {{n}} of {{total}}"}}"#),
    );
    assert_eq!(p["pages"], 3, "numbered 3 pages: {p}");
    let ip = call(office__pdf_info, &format!(r#"{{"path":"{pn}"}}"#));
    assert_eq!(ip["pages"], 3, "numbered pdf still 3 pages");

    for f in [&src, &wm, &pn] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn chart_save_dispatches_by_extension() {
    let spec = r#""type":"bar","series":[{"name":"s","data":[3,6,9]}],"categories":["a","b","c"]"#;
    for (ext, magic) in [("svg", "<svg"), ("png", "PNG"), ("pdf", "%PDF")] {
        let path = tmp(&format!("save.{ext}"));
        let v = call(
            office__chart_save,
            &format!(r#"{{{spec},"path":"{path}"}}"#),
        );
        assert_eq!(v["ok"], true, "{ext}: save failed: {v}");
        let bytes = std::fs::read(&path).unwrap();
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(8)]);
        assert!(head.contains(magic), "{ext}: wrong magic: {head:?}");
        std::fs::remove_file(&path).ok();
    }
}

#[test]
fn sankey_renders_raster_and_svg() {
    let spec = r#""type":"sankey","nodes":[{"name":"A"},{"name":"B"},{"name":"X"},{"name":"Y"}],"links":[{"source":0,"target":2,"value":5},{"source":0,"target":3,"value":3},{"source":1,"target":2,"value":2}]"#;
    // SVG
    let v = call(office__chart_svg, &format!(r#"{{{spec}}}"#));
    assert!(
        v["svg"].as_str().unwrap_or("").contains("<path"),
        "sankey svg has flow paths"
    );
    // Raster handle
    let c = call(
        office__chart_render,
        &format!(r#"{{{spec},"width":500,"height":300}}"#),
    );
    let h = c["handle"].as_u64().expect("sankey raster handle");
    assert_eq!(c["width"], 500);
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
}

#[test]
fn image_color_processing_ops() {
    let n = call(
        office__img_new,
        r#"{"width":40,"height":40,"color":[120,160,200,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    for (f, arg) in [
        (
            office__img_gamma as extern "C" fn(*const c_char) -> *const c_char,
            r#"{"handle":H,"gamma":1.8}"#,
        ),
        (office__img_threshold, r#"{"handle":H,"level":120}"#),
        (office__img_posterize, r#"{"handle":H,"levels":3}"#),
        (office__img_sepia, r#"{"handle":H}"#),
        (office__img_tint, r#"{"handle":H,"color":[255,200,150]}"#),
    ] {
        let v = call(f, &arg.replace('H', &h.to_string()));
        assert_eq!(v["ok"], true, "op failed: {v}");
    }
    let info = call(office__img_info, &format!(r#"{{"handle":{h}}}"#));
    assert_eq!(info["width"], 40, "image intact after color ops");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
}

#[test]
fn image_extended_processing_ops() {
    let n = call(
        office__img_new,
        r#"{"width":48,"height":48,"color":[120,160,200,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    // in-place ops that return {"ok":true}
    for (f, arg) in [
        (
            office__img_autocontrast as extern "C" fn(*const c_char) -> *const c_char,
            r#"{"handle":H,"cutoff":2}"#,
        ),
        (office__img_equalize, r#"{"handle":H}"#),
        (office__img_solarize, r#"{"handle":H,"threshold":100}"#),
        (
            office__img_colorize,
            r##"{"handle":H,"black":"#000080","white":"#ffd700"}"##,
        ),
        (office__img_emboss, r#"{"handle":H}"#),
        (
            office__img_convolve,
            r#"{"handle":H,"kernel":[0,-1,0,-1,5,-1,0,-1,0]}"#,
        ),
        (office__img_box_blur, r#"{"handle":H,"radius":2}"#),
        (office__img_median, r#"{"handle":H,"radius":1}"#),
        (office__img_pixelate, r#"{"handle":H,"block":4}"#),
        (office__img_vignette, r#"{"handle":H,"strength":0.5}"#),
        (office__img_opacity, r#"{"handle":H,"factor":0.5}"#),
        (office__img_putalpha, r#"{"handle":H,"alpha":200}"#),
        (
            office__img_noise,
            r#"{"handle":H,"kind":"gaussian","amount":15,"seed":7}"#,
        ),
        (
            office__img_noise,
            r#"{"handle":H,"kind":"salt_pepper","amount":0.1,"seed":7}"#,
        ),
        (
            office__img_watermark,
            r#"{"handle":H,"text":"DRAFT","opacity":0.3}"#,
        ),
        (office__img_dilate, r#"{"handle":H,"iterations":1}"#),
        (office__img_erode, r#"{"handle":H,"iterations":1}"#),
    ] {
        let v = call(f, &arg.replace('H', &h.to_string()));
        assert_eq!(v["ok"], true, "op failed: {v}");
    }
    // geometry ops that report new dimensions
    let bordered = call(
        office__img_border,
        &format!(r#"{{"handle":{h},"size":5,"color":[255,0,0]}}"#),
    );
    assert_eq!(bordered["width"], 58, "border grows canvas: {bordered}");
    let tp = call(office__img_transpose, &format!(r#"{{"handle":{h}}}"#));
    assert_eq!(tp["width"], 58, "transpose swaps W/H: {tp}");
    let tv = call(office__img_transverse, &format!(r#"{{"handle":{h}}}"#));
    assert_eq!(tv["height"], 58, "transverse swaps W/H back: {tv}");

    // canny edges → grayscale image
    let edges = call(
        office__img_edges,
        &format!(r#"{{"handle":{h},"low":20,"high":80}}"#),
    );
    assert_eq!(edges["mode"], "L", "edges produce grayscale: {edges}");

    // histogram + extrema readouts
    let hist = call(office__img_histogram, &format!(r#"{{"handle":{h}}}"#));
    assert_eq!(
        hist["luma"].as_array().unwrap().len(),
        256,
        "256-bin luma histogram"
    );
    let ext = call(office__img_extrema, &format!(r#"{{"handle":{h}}}"#));
    assert!(
        ext["r"].is_array(),
        "extrema reports per-channel min/max: {ext}"
    );

    // split → 4 channel handles, merge back
    let sp = call(office__img_split, &format!(r#"{{"handle":{h}}}"#));
    let (cr, cg, cb, ca) = (
        sp["handles"]["r"].as_u64().unwrap(),
        sp["handles"]["g"].as_u64().unwrap(),
        sp["handles"]["b"].as_u64().unwrap(),
        sp["handles"]["a"].as_u64().unwrap(),
    );
    let merged = call(
        office__img_merge,
        &format!(r#"{{"r":{cr},"g":{cg},"b":{cb},"a":{ca}}}"#),
    );
    let mh = merged["handle"].as_u64().expect("merge handle");

    // blend / blend_mode / composite between two same-size handles
    let other = call(
        office__img_new,
        r#"{"width":58,"height":58,"color":[200,80,40,255]}"#,
    );
    let oh = other["handle"].as_u64().unwrap();
    let bl = call(
        office__img_blend,
        &format!(r#"{{"handle":{h},"src":{oh},"alpha":0.4}}"#),
    );
    assert_eq!(bl["ok"], true, "blend: {bl}");
    let bm = call(
        office__img_blend_mode,
        &format!(r#"{{"handle":{h},"src":{oh},"mode":"screen"}}"#),
    );
    assert_eq!(bm["ok"], true, "blend_mode: {bm}");

    for handle in [h, cr, cg, cb, ca, mh, oh] {
        call(office__img_close, &format!(r#"{{"handle":{handle}}}"#));
    }
}

#[test]
fn image_animation_drawing_transform_base64() {
    // build 3 distinct frames, write an animated GIF, read frames back
    let mut handles = Vec::new();
    for color in ["[200,0,0,255]", "[0,200,0,255]", "[0,0,200,255]"] {
        let n = call(
            office__img_new,
            &format!(r#"{{"width":32,"height":32,"color":{color}}}"#),
        );
        handles.push(n["handle"].as_u64().unwrap());
    }
    let gif = tmp("anim.gif");
    let hs = format!(
        "[{}]",
        handles
            .iter()
            .map(|h| h.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let w = call(
        office__img_save_animated,
        &format!(r#"{{"path":"{gif}","handles":{hs},"delay":80,"repeat":"infinite"}}"#),
    );
    assert_eq!(w["ok"], true, "save_animated failed: {w}");
    let fr = call(office__img_open_frames, &format!(r#"{{"path":"{gif}"}}"#));
    assert_eq!(fr["count"], 3, "3 frames round-trip: {fr}");
    assert_eq!(fr["frames"][0]["width"], 32, "frame dims preserved");
    for f in fr["frames"].as_array().unwrap() {
        call(
            office__img_close,
            &format!(r#"{{"handle":{}}}"#, f["handle"].as_u64().unwrap()),
        );
    }
    std::fs::remove_file(&gif).ok();

    // montage the 3 source frames into a grid
    let mont = call(
        office__img_montage,
        &format!(r#"{{"handles":{hs},"cols":2,"gap":3}}"#),
    );
    let mh = mont["handle"].as_u64().expect("montage handle");
    assert!(mont["width"].as_u64().unwrap() >= 32, "montage sized");
    call(office__img_close, &format!(r#"{{"handle":{mh}}}"#));

    // advanced drawing + gradient + warp on a fresh canvas
    let base = call(
        office__img_new,
        r#"{"width":64,"height":64,"color":[255,255,255,255]}"#,
    );
    let h = base["handle"].as_u64().unwrap();
    for (f, arg) in [
        (
            office__img_gradient as extern "C" fn(*const c_char) -> *const c_char,
            r##"{"handle":H,"kind":"radial","from":"#ff0000","to":"#0000ff"}"##,
        ),
        (
            office__img_draw_ellipse,
            r#"{"handle":H,"x":32,"y":32,"rx":20,"ry":12,"color":[0,0,0,255]}"#,
        ),
        (
            office__img_draw_polygon,
            r#"{"handle":H,"points":[[5,5],[60,10],[30,55]],"color":[0,128,0,200]}"#,
        ),
        (
            office__img_draw_text_multiline,
            r#"{"handle":H,"x":2,"y":2,"text":"a\nb\nc","size":10,"color":[0,0,0,255]}"#,
        ),
        (
            office__img_warp,
            r#"{"handle":H,"matrix":[1,0.2,0,0,1,0,0,0,1]}"#,
        ),
    ] {
        let v = call(f, &arg.replace('H', &h.to_string()));
        assert_eq!(v["ok"], true, "draw/transform failed: {v}");
    }

    // base64 round-trip: encode → decode → same dimensions
    let enc = call(
        office__img_to_base64,
        &format!(r#"{{"handle":{h},"format":"png"}}"#),
    );
    let b64 = enc["base64"].as_str().expect("base64 string");
    assert!(!b64.is_empty(), "base64 non-empty");
    let dec = call(office__img_from_base64, &format!(r#"{{"base64":"{b64}"}}"#));
    assert_eq!(dec["width"], 64, "base64 decode dims: {dec}");
    let dh = dec["handle"].as_u64().unwrap();

    for handle in handles.iter().chain([&h, &dh]) {
        call(office__img_close, &format!(r#"{{"handle":{handle}}}"#));
    }
}

#[test]
fn image_shapes_fills_masks_analysis() {
    let n = call(
        office__img_new,
        r#"{"width":64,"height":64,"color":[255,255,255,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    for (f, arg) in [
        (
            office__img_draw_rounded_rect as extern "C" fn(*const c_char) -> *const c_char,
            r#"{"handle":H,"x":4,"y":4,"width":40,"height":24,"radius":8,"color":[200,40,40,255]}"#,
        ),
        (
            office__img_draw_polyline,
            r#"{"handle":H,"points":[[2,2],[20,40],[50,10]],"color":[0,0,255,255]}"#,
        ),
        (
            office__img_draw_arc,
            r#"{"handle":H,"x":32,"y":32,"radius":20,"start":0,"end":270,"fill":true,"color":[0,128,0,200]}"#,
        ),
        (
            office__img_replace_color,
            r#"{"handle":H,"from":[255,255,255],"to":[240,240,200],"tolerance":4}"#,
        ),
        (
            office__img_flood_fill,
            r#"{"handle":H,"x":0,"y":0,"color":[180,220,255,255],"tolerance":8}"#,
        ),
        (office__img_swap_channels, r#"{"handle":H,"order":"bgr"}"#),
        (office__img_crop_circle, r#"{"handle":H}"#),
    ] {
        let v = call(f, &arg.replace('H', &h.to_string()));
        assert_eq!(v["ok"], true, "shape/fill op failed: {v}");
    }
    // round_corners + drop_shadow report new geometry
    let rc = call(
        office__img_round_corners,
        &format!(r#"{{"handle":{h},"radius":12}}"#),
    );
    assert_eq!(rc["ok"], true, "round_corners: {rc}");
    let ds = call(
        office__img_drop_shadow,
        &format!(r#"{{"handle":{h},"dx":5,"dy":5,"blur":3}}"#),
    );
    assert!(
        ds["width"].as_u64().unwrap() > 64,
        "drop_shadow grows canvas: {ds}"
    );

    // dominant colors
    let dc = call(
        office__img_dominant_colors,
        &format!(r#"{{"handle":{h},"count":3}}"#),
    );
    assert!(
        dc["colors"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "dominant colors: {dc}"
    );

    // text measurement (no handle)
    let ts = call(office__img_text_size, r#"{"text":"Hello","size":20}"#);
    assert!(
        ts["width"].as_u64().unwrap() > 0,
        "text width measured: {ts}"
    );

    // compare against a copy → identical
    let copy = call(
        office__img_new,
        r#"{"width":64,"height":64,"color":[255,255,255,255]}"#,
    );
    let ch = copy["handle"].as_u64().unwrap();
    let cmp = call(
        office__img_compare,
        &format!(r#"{{"handle":{ch},"other":{ch}}}"#),
    );
    assert_eq!(cmp["identical"], true, "self-compare identical: {cmp}");

    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    call(office__img_close, &format!(r#"{{"handle":{ch}}}"#));
}

#[test]
fn image_color_science_and_distortions() {
    let n = call(
        office__img_new,
        r#"{"width":48,"height":48,"color":[120,160,200,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    for (f, arg) in [
        (
            office__img_levels as extern "C" fn(*const c_char) -> *const c_char,
            r#"{"handle":H,"in_black":20,"in_white":230,"gamma":1.2,"out_black":10,"out_white":250}"#,
        ),
        (
            office__img_curves,
            r#"{"handle":H,"points":[[0,0],[128,180],[255,255]]}"#,
        ),
        (
            office__img_hsl,
            r#"{"handle":H,"hue":40,"saturation":1.3,"lightness":0.95}"#,
        ),
        (office__img_temperature, r#"{"handle":H,"amount":35}"#),
        (
            office__img_channel_mixer,
            r#"{"handle":H,"matrix":[0.5,0.3,0.2,0.1,0.8,0.1,0.2,0.2,0.6]}"#,
        ),
        (office__img_swirl, r#"{"handle":H,"strength":2.5}"#),
        (
            office__img_wave,
            r#"{"handle":H,"amplitude":6,"wavelength":24,"axis":"x"}"#,
        ),
        (office__img_fisheye, r#"{"handle":H,"strength":0.6}"#),
        (office__img_kaleidoscope, r#"{"handle":H,"segments":8}"#),
    ] {
        let v = call(f, &arg.replace('H', &h.to_string()));
        assert_eq!(v["ok"], true, "color/distort op failed: {v}");
    }
    // seam carve reports the new (smaller) width
    let sc = call(
        office__img_seam_carve,
        &format!(r#"{{"handle":{h},"width":36}}"#),
    );
    assert_eq!(sc["width"], 36, "seam carve to target width: {sc}");
    // spritesheet splits into a 2x2 grid of handles
    let n2 = call(
        office__img_new,
        r#"{"width":40,"height":40,"color":[10,20,30,255]}"#,
    );
    let h2 = n2["handle"].as_u64().unwrap();
    let ss = call(
        office__img_spritesheet,
        &format!(r#"{{"handle":{h2},"cols":2,"rows":2}}"#),
    );
    let cells = ss["handles"].as_array().unwrap();
    assert_eq!(cells.len(), 4, "2x2 sprite split: {ss}");
    for c in cells {
        call(
            office__img_close,
            &format!(r#"{{"handle":{}}}"#, c.as_u64().unwrap()),
        );
    }
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    call(office__img_close, &format!(r#"{{"handle":{h2}}}"#));
}

#[test]
fn image_dither_quantize_favicon() {
    // build a gradient so quantization/dither have real color variety
    let n = call(
        office__img_new,
        r#"{"width":64,"height":64,"color":[255,255,255,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    call(
        office__img_gradient,
        &format!(r##"{{"handle":{h},"kind":"linear","from":"#ff0000","to":"#0000ff"}}"##),
    );

    // dither to 1-bit per channel
    let d = call(
        office__img_dither,
        &format!(r#"{{"handle":{h},"levels":2}}"#),
    );
    assert_eq!(d["ok"], true, "dither: {d}");

    // quantize to 8 colors → palette returned
    let q = call(
        office__img_quantize,
        &format!(r#"{{"handle":{h},"colors":8}}"#),
    );
    let colors = q["colors"].as_array().unwrap();
    assert!(
        !colors.is_empty() && colors.len() <= 8,
        "quantize palette size: {}",
        colors.len()
    );
    assert!(
        colors[0]["hex"].as_str().unwrap_or("").starts_with('#'),
        "palette has hex"
    );

    // multi-size favicon
    let path = tmp("fav.ico");
    let f = call(
        office__img_favicon,
        &format!(r#"{{"handle":{h},"path":"{path}","sizes":[16,32,48]}}"#),
    );
    assert_eq!(f["ok"], true, "favicon: {f}");
    let bytes = std::fs::read(&path).unwrap();
    // ICONDIR: reserved=0, type=1, count=3
    assert_eq!(&bytes[0..4], &[0, 0, 1, 0], "ico header");
    assert_eq!(bytes[4], 3, "ico contains 3 images");
    std::fs::remove_file(&path).ok();

    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
}

#[test]
fn chart_radar_and_bubble_render() {
    let c = call(
        office__chart_render,
        r#"{"type":"radar","width":400,"height":400,"categories":["a","b","c","d","e"],"series":[{"name":"s","data":[4,8,2,6,5]}]}"#,
    );
    let h = c["handle"].as_u64().expect("radar handle");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    let v = call(
        office__chart_svg,
        r#"{"type":"radar","categories":["a","b","c"],"series":[{"data":[3,5,2]},{"data":[4,1,6]}]}"#,
    );
    assert!(
        v["svg"].as_str().unwrap_or("").contains("<polygon"),
        "radar svg polygons"
    );
    let cb = call(
        office__chart_render,
        r#"{"type":"bubble","series":[{"data":[[1,2,10],[3,5,30],[6,1,20]]}]}"#,
    );
    let hb = cb["handle"].as_u64().expect("bubble handle");
    call(office__img_close, &format!(r#"{{"handle":{hb}}}"#));
    let vb = call(
        office__chart_svg,
        r#"{"type":"bubble","series":[{"data":[[1,2,10],[3,5,30]]}]}"#,
    );
    assert!(
        vb["svg"].as_str().unwrap_or("").contains("<circle"),
        "bubble svg circles"
    );
}

#[test]
fn chart_new_types_render_raster_and_svg() {
    // cartesian-family new types (need a series)
    let cart = r#"[{"name":"s1","data":[5,8,3,9,4]}]"#;
    for kind in ["step", "waterfall", "boxplot", "combo"] {
        let c = call(
            office__chart_render,
            &format!(
                r#"{{"type":"{kind}","width":420,"height":300,"categories":["a","b","c","d","e"],"series":{cart},"legend":false}}"#
            ),
        );
        let h = c["handle"]
            .as_u64()
            .unwrap_or_else(|| panic!("{kind} raster: {c}"));
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
        let v = call(
            office__chart_svg,
            &format!(r#"{{"type":"{kind}","categories":["a","b","c","d","e"],"series":{cart}}}"#),
        );
        let svg = v["svg"].as_str().unwrap_or("");
        assert!(
            svg.starts_with("<svg") && svg.ends_with("</svg>"),
            "{kind} svg malformed"
        );
    }
    // OHLC + candlestick from [o,h,l,c] tuples
    let ohlc = r#"[{"data":[[10,15,8,12],[12,14,9,10],[10,13,7,13]]}]"#;
    for kind in ["ohlc", "candlestick"] {
        let c = call(
            office__chart_render,
            &format!(r#"{{"type":"{kind}","series":{ohlc}}}"#),
        );
        let h = c["handle"]
            .as_u64()
            .unwrap_or_else(|| panic!("{kind}: {c}"));
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
        let v = call(
            office__chart_svg,
            &format!(r#"{{"type":"{kind}","series":{ohlc}}}"#),
        );
        assert!(
            v["svg"].as_str().unwrap_or("").contains("<line"),
            "{kind} svg wicks"
        );
    }
    // funnel uses categories for labels
    let fc = call(
        office__chart_render,
        r#"{"type":"funnel","categories":["lead","trial","paid"],"series":[{"data":[100,60,25]}],"labels":true}"#,
    );
    let fh = fc["handle"].as_u64().expect("funnel handle");
    call(office__img_close, &format!(r#"{{"handle":{fh}}}"#));
    // gauge + heatmap do NOT require a series
    let g = call(
        office__chart_render,
        r#"{"type":"gauge","value":72,"max":100}"#,
    );
    let gh = g["handle"].as_u64().expect("gauge handle");
    call(office__img_close, &format!(r#"{{"handle":{gh}}}"#));
    let hm = call(
        office__chart_svg,
        r#"{"type":"heatmap","matrix":[[1,2,3],[4,5,6],[7,8,9]],"categories":["x","y","z"]}"#,
    );
    assert!(
        hm["svg"].as_str().unwrap_or("").contains("<rect"),
        "heatmap svg cells"
    );
}

#[test]
fn chart_incr20_types_render_raster_and_svg() {
    // treemap / polar / pareto / stacked_area use a flat series
    let series = r#"[{"name":"s","data":[40,25,15,12,8]}]"#;
    for kind in ["treemap", "polar", "pareto", "stacked_area"] {
        let c = call(
            office__chart_render,
            &format!(
                r#"{{"type":"{kind}","width":420,"height":320,"categories":["a","b","c","d","e"],"series":{series}}}"#
            ),
        );
        let h = c["handle"]
            .as_u64()
            .unwrap_or_else(|| panic!("{kind} raster: {c}"));
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
        let v = call(
            office__chart_svg,
            &format!(r#"{{"type":"{kind}","categories":["a","b","c","d","e"],"series":{series}}}"#),
        );
        let svg = v["svg"].as_str().unwrap_or("");
        assert!(
            svg.starts_with("<svg") && svg.ends_with("</svg>"),
            "{kind} svg malformed"
        );
    }
    // bullet graph: per-series value/target/ranges
    let bullet = r#"[{"name":"Rev","value":270,"target":250,"ranges":[150,220,300]},{"name":"Cost","value":180,"target":200,"ranges":[120,200,260]}]"#;
    let cb = call(
        office__chart_render,
        &format!(r#"{{"type":"bullet","series":{bullet}}}"#),
    );
    let hb = cb["handle"].as_u64().expect("bullet raster handle");
    call(office__img_close, &format!(r#"{{"handle":{hb}}}"#));
    let vb = call(
        office__chart_svg,
        &format!(r#"{{"type":"bullet","series":{bullet}}}"#),
    );
    assert!(
        vb["svg"].as_str().unwrap_or("").contains("<rect"),
        "bullet svg bars"
    );

    // scatter trendline overlay emits a dashed regression line in SVG
    let st = call(
        office__chart_svg,
        r#"{"type":"scatter","series":[{"data":[[1,2],[2,3.1],[3,3.9],[4,5.2],[5,6.1]]}],"trendline":true}"#,
    );
    assert!(
        st["svg"]
            .as_str()
            .unwrap_or("")
            .contains("stroke-dasharray"),
        "trendline dashed line present"
    );
}

#[test]
fn chart_incr22_types_and_overlays() {
    // lollipop / dot are cartesian
    for kind in ["lollipop", "dot"] {
        let c = call(
            office__chart_render,
            &format!(
                r#"{{"type":"{kind}","width":400,"height":300,"categories":["a","b","c"],"series":[{{"data":[5,9,3]}}]}}"#
            ),
        );
        let h = c["handle"]
            .as_u64()
            .unwrap_or_else(|| panic!("{kind}: {c}"));
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
        let v = call(
            office__chart_svg,
            &format!(
                r#"{{"type":"{kind}","categories":["a","b","c"],"series":[{{"data":[5,9,3]}}]}}"#
            ),
        );
        assert!(
            v["svg"].as_str().unwrap_or("").contains("<circle"),
            "{kind} svg dots"
        );
    }
    // gantt: tasks with start/end
    let gantt = r#"[{"name":"design","start":0,"end":3},{"name":"build","start":3,"end":8},{"name":"ship","start":8,"end":10}]"#;
    let gc = call(
        office__chart_render,
        &format!(r#"{{"type":"gantt","series":{gantt}}}"#),
    );
    let gh = gc["handle"].as_u64().expect("gantt raster handle");
    call(office__img_close, &format!(r#"{{"handle":{gh}}}"#));
    let gv = call(
        office__chart_svg,
        &format!(r#"{{"type":"gantt","series":{gantt}}}"#),
    );
    assert!(
        gv["svg"].as_str().unwrap_or("").contains("<rect"),
        "gantt svg bars"
    );
    // sunburst: rings, no series required
    let sv = call(
        office__chart_svg,
        r#"{"type":"sunburst","rings":[[10,20],[5,5,10,10]]}"#,
    );
    assert!(
        sv["svg"].as_str().unwrap_or("").contains("<path"),
        "sunburst svg ring segments"
    );
    let sc = call(
        office__chart_render,
        r#"{"type":"sunburst","rings":[[10,20],[5,5,10,10]]}"#,
    );
    let sh = sc["handle"].as_u64().expect("sunburst raster handle");
    call(office__img_close, &format!(r#"{{"handle":{sh}}}"#));
    // markers + reference lines on a line chart (SVG)
    let lv = call(
        office__chart_svg,
        r#"{"type":"line","categories":["a","b","c"],"series":[{"data":[3,7,5]}],"markers":true,"reference_lines":[{"y":6}]}"#,
    );
    let svg = lv["svg"].as_str().unwrap_or("");
    assert!(svg.contains("<circle"), "line markers present");
    assert!(svg.contains("stroke-dasharray"), "reference line dashed");
}

#[test]
fn chart_incr25_types_theming_smooth() {
    // range bars from [lo,hi] pairs
    let rc = call(
        office__chart_render,
        r#"{"type":"range_column","categories":["a","b","c"],"series":[{"data":[[2,8],[3,6],[1,9]]}]}"#,
    );
    let rh = rc["handle"].as_u64().expect("range raster handle");
    call(office__img_close, &format!(r#"{{"handle":{rh}}}"#));
    let rv = call(
        office__chart_svg,
        r#"{"type":"range_column","categories":["a","b","c"],"series":[{"data":[[2,8],[3,6],[1,9]]}]}"#,
    );
    assert!(
        rv["svg"].as_str().unwrap_or("").contains("<rect"),
        "range svg bars"
    );

    // waffle / slope / percent_stacked / streamgraph
    for kind in ["waffle", "slope", "percent_stacked", "streamgraph"] {
        let series = if kind == "slope" {
            r#"[{"name":"x","data":[10,40]},{"name":"y","data":[30,20]}]"#
        } else {
            r#"[{"name":"s1","data":[5,8,3]},{"name":"s2","data":[2,4,6]}]"#
        };
        let c = call(
            office__chart_render,
            &format!(
                r#"{{"type":"{kind}","width":400,"height":320,"categories":["a","b","c"],"series":{series}}}"#
            ),
        );
        let h = c["handle"]
            .as_u64()
            .unwrap_or_else(|| panic!("{kind} raster: {c}"));
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
        let v = call(
            office__chart_svg,
            &format!(r#"{{"type":"{kind}","categories":["a","b","c"],"series":{series}}}"#),
        );
        let svg = v["svg"].as_str().unwrap_or("");
        assert!(
            svg.starts_with("<svg") && svg.ends_with("</svg>"),
            "{kind} svg malformed"
        );
    }

    // custom palette + background + smooth line
    let v = call(
        office__chart_svg,
        r##"{"type":"line","categories":["a","b","c","d"],"series":[{"data":[3,9,2,7]}],"smooth":true,"palette":["#112233"],"background":"#101820"}"##,
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(svg.contains("#101820"), "custom background applied");
    assert!(svg.contains("#112233"), "custom palette applied");
}

#[test]
fn chart_log_axis_errorbars_annotations() {
    // log_y scale on a wide-range line
    let lv = call(
        office__chart_svg,
        r#"{"type":"line","categories":["a","b","c","d"],"series":[{"data":[1,10,100,1000]}],"log_y":true}"#,
    );
    let svg = lv["svg"].as_str().unwrap_or("");
    assert!(svg.starts_with("<svg"), "log_y svg renders");
    let lc = call(
        office__chart_render,
        r#"{"type":"line","series":[{"data":[1,10,100,1000]}],"log_y":true}"#,
    );
    let lh = lc["handle"].as_u64().expect("log_y raster handle");
    call(office__img_close, &format!(r#"{{"handle":{lh}}}"#));

    // error bars on a bar chart (SVG): whiskers drawn
    let ev = call(
        office__chart_svg,
        r#"{"type":"bar","categories":["a","b","c"],"series":[{"data":[5,8,3],"errors":[1,1.5,0.5]}]}"#,
    );
    assert!(
        ev["svg"].as_str().unwrap_or("").matches("<line").count() >= 3,
        "error-bar whiskers present"
    );

    // annotations marker + text
    let av = call(
        office__chart_svg,
        r#"{"type":"line","categories":["a","b","c","d"],"series":[{"data":[3,7,5,9]}],"annotations":[{"x":1,"y":7,"text":"peak"}]}"#,
    );
    let asvg = av["svg"].as_str().unwrap_or("");
    assert!(asvg.contains(">peak<"), "annotation text present");
    assert!(asvg.contains("<circle"), "annotation marker present");
}

#[test]
fn chart_grid_dashboard_and_new_types() {
    // marimekko + radial_bar render in both renderers
    for kind in ["marimekko", "radial_bar"] {
        let series = r#"[{"name":"s1","data":[5,8,3,6]},{"name":"s2","data":[2,4,6,1]}]"#;
        let c = call(
            office__chart_render,
            &format!(
                r#"{{"type":"{kind}","width":400,"height":320,"categories":["a","b","c","d"],"series":{series}}}"#
            ),
        );
        let h = c["handle"]
            .as_u64()
            .unwrap_or_else(|| panic!("{kind} raster: {c}"));
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
        let v = call(
            office__chart_svg,
            &format!(r#"{{"type":"{kind}","categories":["a","b","c","d"],"series":{series}}}"#),
        );
        assert!(
            v["svg"].as_str().unwrap_or("").starts_with("<svg"),
            "{kind} svg"
        );
    }

    // dashboard grid: 4 different charts tiled into one image, returns a handle
    let charts = r#"[
        {"type":"bar","categories":["a","b"],"series":[{"data":[3,7]}]},
        {"type":"line","categories":["a","b","c"],"series":[{"data":[2,5,3]}]},
        {"type":"pie","categories":["x","y"],"series":[{"data":[6,4]}]},
        {"type":"sankey","links":[{"source":0,"target":1,"value":5}]}
    ]"#;
    let g = call(
        office__chart_grid,
        &format!(r#"{{"charts":{charts},"cols":2,"cell_width":300,"cell_height":220,"gap":8}}"#),
    );
    assert_eq!(g["charts"], 4, "grid composited 4 charts: {g}");
    let gh = g["handle"].as_u64().expect("grid handle");
    assert!(
        g["width"].as_u64().unwrap() >= 600,
        "grid width spans 2 columns"
    );
    call(office__img_close, &format!(r#"{{"handle":{gh}}}"#));

    // grid saved straight to PDF
    let path = tmp("dash.pdf");
    let gp = call(
        office__chart_grid,
        &format!(r#"{{"charts":{charts},"cols":2,"path":"{path}"}}"#),
    );
    assert_eq!(gp["ok"], true, "grid pdf save: {gp}");
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        String::from_utf8_lossy(&bytes[..bytes.len().min(8)]).contains("%PDF"),
        "grid pdf magic"
    );
    if let Some(h) = gp["handle"].as_u64() {
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn chart_incr32_calendar_parallel_hexbin() {
    // calendar: needs no series, just values
    let cal = call(
        office__chart_render,
        r#"{"type":"calendar","width":420,"height":160,"values":[1,3,0,5,2,8,4,1,0,6,3,2,7,1,5,2,0,9,3,1]}"#,
    );
    let ch = cal["handle"].as_u64().expect("calendar handle");
    call(office__img_close, &format!(r#"{{"handle":{ch}}}"#));
    let cv = call(
        office__chart_svg,
        r#"{"type":"calendar","values":[1,3,0,5,2,8,4,1,0,6]}"#,
    );
    assert!(
        cv["svg"].as_str().unwrap_or("").contains("<rect"),
        "calendar svg cells"
    );

    // parallel coordinates: each series = a row across dimensions
    let par = r#"[{"data":[5,20,3,80]},{"data":[8,12,9,40]},{"data":[2,18,6,60]}]"#;
    let pc = call(
        office__chart_render,
        &format!(
            r#"{{"type":"parallel","width":420,"height":300,"categories":["a","b","c","d"],"series":{par}}}"#
        ),
    );
    let ph = pc["handle"].as_u64().expect("parallel handle");
    call(office__img_close, &format!(r#"{{"handle":{ph}}}"#));
    let pv = call(
        office__chart_svg,
        &format!(r#"{{"type":"parallel","categories":["a","b","c","d"],"series":{par}}}"#),
    );
    assert!(
        pv["svg"].as_str().unwrap_or("").contains("<polyline"),
        "parallel svg lines"
    );

    // hexbin: scatter points binned
    let hx = r#"[{"data":[[1,2],[1.1,2.1],[1,2.05],[5,9],[5.1,9],[3,3],[3,3.1],[3.05,3]]}]"#;
    let hc = call(
        office__chart_render,
        &format!(r#"{{"type":"hexbin","width":400,"height":400,"radius":20,"series":{hx}}}"#),
    );
    let hh = hc["handle"].as_u64().expect("hexbin handle");
    call(office__img_close, &format!(r#"{{"handle":{hh}}}"#));
    let hv = call(
        office__chart_svg,
        &format!(r#"{{"type":"hexbin","radius":20,"series":{hx}}}"#),
    );
    assert!(
        hv["svg"].as_str().unwrap_or("").contains("<polygon"),
        "hexbin svg cells"
    );
}

#[test]
fn chart_legend_and_labels_emit() {
    // legend swatches + value labels appear in the SVG
    let v = call(
        office__chart_svg,
        r#"{"type":"bar","categories":["a","b","c"],"series":[{"name":"Alpha","data":[3,6,9]},{"name":"Beta","data":[2,5,1]}],"labels":true,"x_label":"qtr","y_label":"units"}"#,
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.contains(">Alpha<"),
        "legend series name present: {}",
        &svg[..svg.len().min(80)]
    );
    assert!(svg.contains("rotate(-90"), "rotated y-axis title present");
    assert!(svg.contains(">qtr<"), "x-axis title present");
}

#[test]
fn chart_save_new_type_any_format() {
    // a brand-new type round-trips through chart_save's extension dispatch
    let spec =
        r#""type":"waterfall","series":[{"data":[10,-3,5,-2]}],"categories":["a","b","c","d"]"#;
    for (ext, magic) in [("svg", "<svg"), ("png", "PNG"), ("pdf", "%PDF")] {
        let path = tmp(&format!("wf.{ext}"));
        let v = call(
            office__chart_save,
            &format!(r#"{{{spec},"path":"{path}"}}"#),
        );
        assert_eq!(v["ok"], true, "{ext}: {v}");
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            String::from_utf8_lossy(&bytes[..bytes.len().min(8)]).contains(magic),
            "{ext} magic"
        );
        std::fs::remove_file(&path).ok();
    }
}

#[test]
fn xlsx_conditional_format_and_validation() {
    let path = tmp("cv.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r##"{{"path":"{path}","sheets":[{{
                "name":"S",
                "rows":[["score"],[95],[40],[70]],
                "conditional":[{{"range":[1,0,3,0],"rule":"greater_than","value":80,"format":{{"bold":true,"bg":"#C6EFCE"}}}}],
                "validations":[{{"range":[1,1,3,1],"list":["yes","no","maybe"]}}]
            }}]}}"##
        ),
    );
    assert_eq!(w["ok"], true, "conditional/validation write failed: {w}");
    let r = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(
        r["sheets"][0]["rows"][1][0], 95.0,
        "data intact with conditional+validation"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn xlsx_advanced_setup_writes_and_data_round_trips() {
    let path = tmp("setup.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r##"{{"path":"{path}",
                "defined_names":[{{"name":"Region","formula":"=Data!$A$1"}}],
                "sheets":[{{
                    "name":"Data",
                    "rows":[["city","sales"],["NYC",100],["LA",80]],
                    "protect":true,"landscape":true,"tab_color":"#FF8800","zoom":120,
                    "print_gridlines":true,"paper":9,
                    "header":"&CQuarterly","footer":"&Lpage &P",
                    "print_area":[0,0,2,1],"repeat_rows":[0,0],
                    "margins":[0.5,0.5,0.6,0.6,0.3,0.3],
                    "notes":[{{"row":1,"col":1,"text":"top city","author":"qa"}}]
                }}]}}"##
        ),
    );
    assert_eq!(w["ok"], true, "advanced setup write failed: {w}");
    // The data still parses back (setup metadata doesn't disturb cell values).
    let r = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(
        r["sheets"][0]["rows"][1][0], "NYC",
        "data intact with setup"
    );
    assert_eq!(
        r["sheets"][0]["rows"][1][1], 100.0,
        "numeric intact with setup"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn xlsx_sparklines_grouping_hide_autofit() {
    let path = tmp("spark.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{
                "name":"S",
                "rows":[["q1","q2","q3","q4","trend"],[3,7,2,9,null],[5,1,8,4,null]],
                "sparklines":[
                    {{"at":[1,4],"range":[1,0,1,3],"type":"line","markers":true,"high":true,"low":true}},
                    {{"at":[2,4],"range":[2,0,2,3],"type":"column"}}
                ],
                "group_rows":[[1,2]],"group_columns":[[0,3]],
                "hide_rows":[2],"hide_columns":[3],"autofit":true
            }}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "sparkline/group write failed: {w}");
    let r = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(
        r["sheets"][0]["rows"][1][0], 3.0,
        "data intact with sparklines/grouping"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn xlsx_properties_and_rich_strings() {
    let path = tmp("props.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r##"{{"path":"{path}",
                "properties":{{"title":"Q Report","author":"jane","company":"MenkeTech","subject":"sales"}},
                "sheets":[{{"name":"S","rows":[
                    [{{"rich":[{{"text":"Hello "}},{{"text":"World","bold":true,"color":"#FF0000"}}]}}]
                ]}}]}}"##
        ),
    );
    assert_eq!(w["ok"], true, "properties/rich write failed: {w}");
    let r = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(
        r["sheets"][0]["rows"][0][0], "Hello World",
        "rich string concatenates on read"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn docx_styled_table_round_trips() {
    let path = tmp("styled_table.docx");
    let w = call(
        office__doc_write,
        &format!(
            r##"{{"path":"{path}","blocks":[
                {{"kind":"table","rows":[
                    [{{"text":"Header","bold":true,"bg":"#D9E1F2","span":2,"valign":"center"}}],
                    [{{"text":"A","width":2400}},{{"text":"B"}}]
                ]}}
            ]}}"##
        ),
    );
    assert_eq!(w["ok"], true, "styled table write failed: {w}");
    let r = call(office__doc_read, &format!(r#"{{"path":"{path}"}}"#));
    let joined: String = r["paragraphs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("Header") && joined.contains("A"),
        "styled table cells round-trip: {joined:?}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn xlsx_formula_write_then_read() {
    let path = tmp("formula.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[
                [10,20,{{"f":"=A1+B1"}}]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "formula write failed: {w}");
    let r = call(
        office__sheet_read,
        &format!(r#"{{"path":"{path}","formulas":true}}"#),
    );
    let f = r["sheets"][0]["formulas"][0][2].as_str().unwrap_or("");
    assert!(
        f.contains("A1") && f.contains("B1"),
        "formula round-trips, got: {f:?}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn docx_lists_links_headers_round_trip() {
    let path = tmp("rich_lists.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{path}","header":"My Report","footer":"confidential","blocks":[
                {{"kind":"heading","level":1,"text":"Agenda"}},
                {{"kind":"list","ordered":true,"items":["First point","Second point"]}},
                {{"kind":"list","ordered":false,"items":["bullet a","bullet b"]}},
                {{"kind":"link","url":"https://example.com","text":"see site"}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "docx rich write failed: {w}");
    let r = call(office__doc_read, &format!(r#"{{"path":"{path}"}}"#));
    let paras = r["paragraphs"].as_array().unwrap();
    let joined: String = paras
        .iter()
        .filter_map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("First point"),
        "ordered list item present: {joined:?}"
    );
    assert!(joined.contains("bullet a"), "bullet list item present");
    assert!(joined.contains("see site"), "hyperlink text present");
    std::fs::remove_file(&path).ok();
}

#[test]
fn chart_missing_series_errors() {
    let v = call(office__chart_render, r#"{"type":"bar"}"#);
    assert!(
        err_of(&v).starts_with("missing series"),
        "got: {}",
        err_of(&v)
    );
}

#[test]
fn image_unknown_handle_errors() {
    let v = call(office__img_save, r#"{"handle":987654,"path":"/tmp/x.png"}"#);
    assert!(
        err_of(&v).starts_with("unknown image handle"),
        "got: {}",
        err_of(&v)
    );
}

#[test]
fn barcode_qr_renders_and_saves() {
    // QR -> handle; module count is the canonical 21..177 odd square side.
    let v = call(
        office__barcode_qr,
        r#"{"data":"https://menketechnologies.github.io","ec":"Q","scale":4,"quiet":2}"#,
    );
    let n = v["modules"].as_u64().expect("modules");
    assert!((21..=177).contains(&n), "qr side out of range: {n}");
    let w = v["width"].as_u64().unwrap();
    assert_eq!(w, (n + 4) * 4, "width = (modules + 2*quiet) * scale");
    let h = v["handle"].as_u64().expect("qr handle");

    // round-trips through the shared image surface (save as png)
    let path = tmp("qr.png");
    let sv = call(
        office__img_save,
        &format!(r#"{{"handle":{h},"path":"{path}"}}"#),
    );
    assert_eq!(sv["ok"], true, "{sv}");
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[1..4], b"PNG", "png magic");
    std::fs::remove_file(&path).ok();
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
}

#[test]
fn barcode_1d_symbologies() {
    // (symbology, data) pairs that each library accepts.
    let cases = [
        ("code128", "STRYKE-2026"),
        ("code39", "ABC123"),
        ("code93", "HELLO"),
        ("ean13", "750103131130"),
        ("ean8", "1234567"),
        ("upca", "03600029145"),
        ("itf", "123456"),
    ];
    for (sym, data) in cases {
        let v = call(
            office__barcode_1d,
            &format!(r#"{{"symbology":"{sym}","data":"{data}","scale":2,"height":60}}"#),
        );
        let h = v["handle"].as_u64().unwrap_or_else(|| panic!("{sym}: {v}"));
        assert_eq!(v["height"].as_u64(), Some(60), "{sym} height");
        assert!(v["bars"].as_u64().unwrap_or(0) > 0, "{sym} has bars");
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    }
}

#[test]
fn meta_xlsx_round_trips() {
    let path = tmp("meta.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[["a",1]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "{w}");
    let m = call(
        office__meta_write,
        &format!(
            r#"{{"path":"{path}","props":{{"title":"Q2 Report","author":"Jane","subject":"sales","keywords":"q2,sales","company":"MenkeTechnologies"}}}}"#
        ),
    );
    assert_eq!(m["ok"], true, "meta_write: {m}");
    let r = call(office__meta_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["title"], "Q2 Report", "{r}");
    assert_eq!(r["author"], "Jane");
    assert_eq!(r["subject"], "sales");
    assert_eq!(r["keywords"], "q2,sales");
    assert_eq!(r["company"], "MenkeTechnologies", "app.xml company: {r}");
    // file is still a readable workbook after the rewrite
    let s = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(s["sheets"][0]["rows"][0][1], 1.0, "data intact: {s}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn meta_docx_merges_without_clobber() {
    let path = tmp("meta.docx");
    call(
        office__doc_write,
        &format!(r#"{{"path":"{path}","blocks":[{{"kind":"para","text":"hi"}}]}}"#),
    );
    // set title only ...
    call(
        office__meta_write,
        &format!(r#"{{"path":"{path}","props":{{"title":"First"}}}}"#),
    );
    // ... then author only; title must survive the second rewrite
    call(
        office__meta_write,
        &format!(r#"{{"path":"{path}","props":{{"author":"JM"}}}}"#),
    );
    let r = call(office__meta_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["title"], "First", "title preserved across merge: {r}");
    assert_eq!(r["author"], "JM", "{r}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn meta_pdf_info_dict() {
    let path = tmp("meta.pdf");
    call(
        office__pdf_write,
        &format!(r#"{{"path":"{path}","lines":["body"]}}"#),
    );
    let out = tmp("meta-out.pdf");
    let m = call(
        office__meta_write,
        &format!(
            r#"{{"path":"{path}","output":"{out}","props":{{"title":"Spec","author":"JM","created":"2026-06-13T12:00:00Z"}}}}"#
        ),
    );
    assert_eq!(m["ok"], true, "{m}");
    let r = call(office__meta_read, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(r["title"], "Spec", "{r}");
    assert_eq!(r["author"], "JM");
    assert_eq!(r["created"], "D:20260613120000", "iso -> pdf date: {r}");
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn meta_ods_round_trips() {
    let path = tmp("meta.ods");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[["a","b"]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "{w}");
    let m = call(
        office__meta_write,
        &format!(r#"{{"path":"{path}","props":{{"title":"Ledger","author":"JM"}}}}"#),
    );
    assert_eq!(m["ok"], true, "meta_write ods: {m}");
    let r = call(office__meta_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["title"], "Ledger", "{r}");
    assert_eq!(r["author"], "JM");
    // still a readable spreadsheet
    let s = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(s["sheets"][0]["rows"][0][0], "a", "ods data intact: {s}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn extract_images_from_pdf() {
    // build a PDF embedding a rendered chart (DCTDecode JPEG XObject)
    let chart = call(
        office__chart_render,
        r#"{"type":"bar","width":200,"height":150,"categories":["a","b"],"series":[{"data":[3,7]}]}"#,
    );
    let ch = chart["handle"].as_u64().unwrap();
    let path = tmp("imgs.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{path}","elements":[{{"type":"image","handle":{ch},"width":180,"height":135}}]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "{b}");

    let r = call(office__extract_images, &format!(r#"{{"path":"{path}"}}"#));
    assert!(
        r["count"].as_u64().unwrap_or(0) >= 1,
        "extracted at least one: {r}"
    );
    let first = &r["images"][0];
    let h = first["handle"].as_u64().expect("extracted handle");
    assert!(
        first["width"].as_u64().unwrap() > 0,
        "decoded dims: {first}"
    );
    // the extracted handle is a live image: re-save it
    let out = tmp("extracted.png");
    let s = call(
        office__img_save,
        &format!(r#"{{"handle":{h},"path":"{out}"}}"#),
    );
    assert_eq!(s["ok"], true, "{s}");
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn extract_images_from_docx_to_dir() {
    // make a real PNG, embed it in a docx, then extract it back out to a dir
    let png = tmp("logo.png");
    let n = call(
        office__img_new,
        r#"{"width":32,"height":24,"color":[0,128,255,255]}"#,
    );
    let nh = n["handle"].as_u64().unwrap();
    call(
        office__img_save,
        &format!(r#"{{"handle":{nh},"path":"{png}"}}"#),
    );

    let docx = tmp("withimg.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{docx}","blocks":[{{"kind":"para","text":"see logo"}},{{"kind":"image","path":"{png}","width":32,"height":24}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "docx write: {w}");

    let dir = tmp("extracted_dir");
    let r = call(
        office__extract_images,
        &format!(r#"{{"path":"{docx}","dir":"{dir}"}}"#),
    );
    assert!(
        r["count"].as_u64().unwrap_or(0) >= 1,
        "media extracted: {r}"
    );
    let saved = r["images"][0]["path"].as_str().expect("saved path");
    assert!(
        std::path::Path::new(saved).exists(),
        "file written: {saved}"
    );
    std::fs::remove_file(&png).ok();
    std::fs::remove_file(&docx).ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// Build a minimal one-page PDF with an AcroForm holding a text field "name"
/// and a checkbox "agree", using lopdf directly (no writer produces forms).
fn make_form_pdf(path: &str) {
    use lopdf::Dictionary;
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let name_field = {
        let mut d = Dictionary::new();
        d.set("Type", Object::Name(b"Annot".to_vec()));
        d.set("Subtype", Object::Name(b"Widget".to_vec()));
        d.set("FT", Object::Name(b"Tx".to_vec()));
        d.set("T", Object::string_literal("name"));
        d.set("V", Object::string_literal(""));
        d.set("Parent", Object::Reference(pages_id)); // placeholder, page set below
        d.set(
            "Rect",
            Object::Array(vec![100.into(), 700.into(), 300.into(), 720.into()]),
        );
        doc.add_object(d)
    };
    let agree_field = {
        let mut ap_n = Dictionary::new();
        ap_n.set("On", Object::Null);
        ap_n.set("Off", Object::Null);
        let mut ap = Dictionary::new();
        ap.set("N", Object::Dictionary(ap_n));
        let mut d = Dictionary::new();
        d.set("Type", Object::Name(b"Annot".to_vec()));
        d.set("Subtype", Object::Name(b"Widget".to_vec()));
        d.set("FT", Object::Name(b"Btn".to_vec()));
        d.set("T", Object::string_literal("agree"));
        d.set("V", Object::Name(b"Off".to_vec()));
        d.set("AS", Object::Name(b"Off".to_vec()));
        d.set("AP", Object::Dictionary(ap));
        d.set(
            "Rect",
            Object::Array(vec![100.into(), 660.into(), 116.into(), 676.into()]),
        );
        doc.add_object(d)
    };

    let mut page = Dictionary::new();
    page.set("Type", Object::Name(b"Page".to_vec()));
    page.set("Parent", Object::Reference(pages_id));
    page.set(
        "MediaBox",
        Object::Array(vec![0.into(), 0.into(), 612.into(), 792.into()]),
    );
    page.set(
        "Annots",
        Object::Array(vec![
            Object::Reference(name_field),
            Object::Reference(agree_field),
        ]),
    );
    let page_id = doc.add_object(page);

    let mut pages = Dictionary::new();
    pages.set("Type", Object::Name(b"Pages".to_vec()));
    pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
    pages.set("Count", 1);
    doc.objects.insert(pages_id, Object::Dictionary(pages));

    let mut acro = Dictionary::new();
    acro.set(
        "Fields",
        Object::Array(vec![
            Object::Reference(name_field),
            Object::Reference(agree_field),
        ]),
    );
    let acro_id = doc.add_object(acro);

    let mut cat = Dictionary::new();
    cat.set("Type", Object::Name(b"Catalog".to_vec()));
    cat.set("Pages", Object::Reference(pages_id));
    cat.set("AcroForm", Object::Reference(acro_id));
    let cat_id = doc.add_object(cat);
    doc.trailer.set("Root", Object::Reference(cat_id));
    doc.save(path).unwrap();
}

#[test]
fn pdf_form_fields_list_and_fill() {
    let path = tmp("form.pdf");
    make_form_pdf(&path);

    // list
    let r = call(office__pdf_form_fields, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["count"], 2, "two fields: {r}");
    let names: Vec<&str> = r["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"name") && names.contains(&"agree"),
        "{names:?}"
    );

    // fill
    let out = tmp("form-filled.pdf");
    let f = call(
        office__pdf_fill_form,
        &format!(
            r#"{{"path":"{path}","output":"{out}","values":{{"name":"Jane Doe","agree":true}}}}"#
        ),
    );
    assert_eq!(f["ok"], true, "{f}");
    assert_eq!(f["filled"], 2, "{f}");

    // read the filled values back
    let r2 = call(office__pdf_form_fields, &format!(r#"{{"path":"{out}"}}"#));
    let name_v = r2["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "name")
        .and_then(|f| f["value"].as_str())
        .unwrap();
    assert_eq!(name_v, "Jane Doe", "text field filled: {r2}");
    let agree_v = r2["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "agree")
        .and_then(|f| f["value"].as_str())
        .unwrap();
    assert_eq!(agree_v, "On", "checkbox set to its on-state: {r2}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn pdf_outline_set_and_read() {
    // a 3-page document
    let path = tmp("outline.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{path}","elements":[
                {{"type":"heading","level":1,"text":"One"}},{{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Two"}},{{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Three"}}
            ]}}"#
        ),
    );
    assert!(b["pages"].as_u64().unwrap() >= 3, "3 pages: {b}");

    let out = tmp("outline-set.pdf");
    let s = call(
        office__pdf_set_outline,
        &format!(
            r#"{{"path":"{path}","output":"{out}","outline":[
                {{"title":"Intro","page":1}},
                {{"title":"Body","page":2,"bold":true,"children":[
                    {{"title":"Detail","page":3}}
                ]}}
            ]}}"#
        ),
    );
    assert_eq!(s["ok"], true, "{s}");
    assert_eq!(s["count"], 3, "three bookmarks total: {s}");

    let r = call(office__pdf_outline, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(r["count"], 3, "{r}");
    let top = r["outline"].as_array().unwrap();
    assert_eq!(top.len(), 2, "two top-level: {r}");
    assert_eq!(top[0]["title"], "Intro");
    assert_eq!(top[0]["page"], 1);
    assert_eq!(top[1]["title"], "Body");
    assert_eq!(top[1]["page"], 2);
    let kids = top[1]["children"].as_array().expect("nested children");
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0]["title"], "Detail");
    assert_eq!(kids[0]["page"], 3, "child dest resolves to page 3: {r}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn pdf_outline_empty_when_none() {
    let path = tmp("noout.pdf");
    call(
        office__pdf_write,
        &format!(r#"{{"path":"{path}","lines":["x"]}}"#),
    );
    let r = call(office__pdf_outline, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["count"], 0, "{r}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn pdf_form_no_acroform() {
    // a plain PDF has no form -> empty list, and fill errors
    let path = tmp("plain.pdf");
    call(
        office__pdf_write,
        &format!(r#"{{"path":"{path}","lines":["hi"]}}"#),
    );
    let r = call(office__pdf_form_fields, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["count"], 0, "no fields: {r}");
    let f = call(
        office__pdf_fill_form,
        &format!(r#"{{"path":"{path}","values":{{"x":"y"}}}}"#),
    );
    assert!(err_of(&f).contains("no AcroForm"), "got: {}", err_of(&f));
    std::fs::remove_file(&path).ok();
}

#[test]
fn replace_text_coalesces_split_runs() {
    // A placeholder split across two w:t runs (the OOXML failure mode a naive
    // per-node replace can't handle) must still be matched and filled.
    let xml = br#"<w:document><w:body><w:p><w:r><w:t>Hello {{na</w:t></w:r><w:r><w:t xml:space="preserve">me}}!</w:t></w:r></w:p></w:body></w:document>"#;
    let (out, n) = replace_in_xml(
        xml,
        "w:p",
        Some("w:t"),
        &[("{{name}}".to_string(), "Jane".to_string())],
    );
    assert_eq!(n, 1, "one substitution");
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("Hello Jane!"), "joined into first run: {s}");
    // a paragraph with no match is left intact (both runs preserved)
    let (out2, n2) = replace_in_xml(xml, "w:p", Some("w:t"), &[("zzz".into(), "q".into())]);
    assert_eq!(n2, 0);
    let s2 = String::from_utf8(out2).unwrap();
    assert!(
        s2.contains("{{na") && s2.contains("me}}!"),
        "untouched: {s2}"
    );
}

#[test]
fn replace_text_docx_round_trips() {
    let path = tmp("tmpl.docx");
    call(
        office__doc_write,
        &format!(
            r#"{{"path":"{path}","blocks":[{{"kind":"para","text":"Dear {{{{name}}}}, your invoice {{{{id}}}} is ready."}}]}}"#
        ),
    );
    let r = call(
        office__replace_text,
        &format!(
            r#"{{"path":"{path}","replace":{{"{{{{name}}}}":"Jane","{{{{id}}}}":"INV-42"}}}}"#
        ),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["replaced"], 2, "two placeholders: {r}");
    let d = call(office__doc_read, &format!(r#"{{"path":"{path}"}}"#));
    let joined = d["paragraphs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        joined.contains("Dear Jane, your invoice INV-42 is ready."),
        "{joined}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn replace_text_xlsx_strings() {
    let path = tmp("tmpl.xlsx");
    call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[["Hi {{who}}",1]]}}]}}"#),
    );
    let r = call(
        office__replace_text,
        &format!(r#"{{"path":"{path}","replace":{{"{{who}}":"World"}}}}"#),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert!(r["replaced"].as_u64().unwrap() >= 1, "{r}");
    let s = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(s["sheets"][0]["rows"][0][0], "Hi World", "{s}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn replace_text_unsupported_format() {
    let path = tmp("x.pdf");
    std::fs::write(&path, "%PDF-1.4\n").ok();
    let r = call(
        office__replace_text,
        &format!(r#"{{"path":"{path}","replace":{{"a":"b"}}}}"#),
    );
    assert!(
        err_of(&r).contains("unsupported format"),
        "got: {}",
        err_of(&r)
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn extract_images_unsupported_format() {
    let path = tmp("x.txt");
    std::fs::write(&path, "x").ok();
    let r = call(office__extract_images, &format!(r#"{{"path":"{path}"}}"#));
    assert!(
        err_of(&r).contains("unsupported format"),
        "got: {}",
        err_of(&r)
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn meta_unsupported_format_errors() {
    let path = tmp("meta.txt");
    std::fs::write(&path, "x").ok();
    let r = call(office__meta_read, &format!(r#"{{"path":"{path}"}}"#));
    assert!(
        err_of(&r).contains("unsupported format"),
        "got: {}",
        err_of(&r)
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn barcode_invalid_inputs_error() {
    // EAN-13 needs digits only -> error surfaced, not a panic.
    let v = call(
        office__barcode_1d,
        r#"{"symbology":"ean13","data":"not-digits"}"#,
    );
    assert!(!err_of(&v).is_empty(), "expected error, got: {v}");
    // unknown symbology name
    let u = call(office__barcode_1d, r#"{"symbology":"nope","data":"x"}"#);
    assert!(
        err_of(&u).contains("unknown symbology"),
        "got: {}",
        err_of(&u)
    );
}
