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
