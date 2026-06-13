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
fn image_unknown_handle_errors() {
    let v = call(office__img_save, r#"{"handle":987654,"path":"/tmp/x.png"}"#);
    assert!(
        err_of(&v).starts_with("unknown image handle"),
        "got: {}",
        err_of(&v)
    );
}
