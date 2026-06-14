// Barcode + QR-code generation. Both produce a `DynamicImage` registered in the
// shared image-handle table (see image_ops.rs), so the result composes with the
// entire image surface: save to any raster format, paste onto a label canvas,
// embed in a PDF (`office__pdf_build`) or a docx — no separate output path.
//
//   * QR codes      — `qrcode` crate, module matrix rendered by hand (we avoid
//                     its `image` feature so it never pins a second image-crate
//                     version into the build).
//   * 1D barcodes   — `barcoders` crate; `encode()` returns a 0/1 column vector
//                     that we draw as full-height vertical bars.

use barcoders::sym::codabar::Codabar;
use barcoders::sym::code11::Code11;
use barcoders::sym::code128::Code128;
use barcoders::sym::code39::Code39;
use barcoders::sym::code93::Code93;
use barcoders::sym::ean13::EAN13;
use barcoders::sym::ean8::EAN8;
use barcoders::sym::tf::TF;

fn opt_u32(v: &Value, key: &str, default: u32) -> u32 {
    v.get(key)
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(default)
}

/// QR code -> image handle.
///
/// opts: `data` (required string), `ec` ("L"|"M"|"Q"|"H", default "M"),
/// `scale` (px per module, default 6), `quiet`/`border` (quiet-zone modules,
/// default 4), `fg`/`bg` (colors, default black on white).
fn op_barcode_qr(opts: Value) -> Result<Value> {
    use qrcode::{Color, EcLevel, QrCode};
    let data = req_str(&opts, "data")?;
    let ec = match opts
        .get("ec")
        .and_then(Value::as_str)
        .unwrap_or("M")
        .to_ascii_uppercase()
        .as_str()
    {
        "L" => EcLevel::L,
        "Q" => EcLevel::Q,
        "H" => EcLevel::H,
        _ => EcLevel::M,
    };
    let scale = opt_u32(&opts, "scale", 6).max(1);
    let quiet = opts
        .get("quiet")
        .or_else(|| opts.get("border"))
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(4);
    let fg = parse_color(opts.get("fg"));
    let bg = parse_color(opts.get("bg").or_else(|| opts.get("background")));

    let code = QrCode::with_error_correction_level(data.as_bytes(), ec)
        .map_err(|e| anyhow!("qr: {e}"))?;
    let n = code.width() as u32; // modules per side (square)
    let colors = code.to_colors(); // row-major, length n*n
    let side = (n + 2 * quiet) * scale;
    let mut img = image::RgbaImage::from_pixel(side, side, bg);
    for my in 0..n {
        for mx in 0..n {
            if colors[(my * n + mx) as usize] == Color::Dark {
                let x0 = (mx + quiet) * scale;
                let y0 = (my + quiet) * scale;
                for dy in 0..scale {
                    for dx in 0..scale {
                        img.put_pixel(x0 + dx, y0 + dy, fg);
                    }
                }
            }
        }
    }
    let (w, h) = (img.width(), img.height());
    let handle = insert_image(image::DynamicImage::ImageRgba8(img));
    Ok(json!({"handle": handle, "width": w, "height": h, "modules": n}))
}

/// Encode a 1D symbology to its 0/1 column vector. Maps friendly names onto the
/// barcoders symbology types; UPC-A is EAN-13 with a leading zero.
fn encode_1d(sym: &str, data: &str, code128_set: char) -> Result<Vec<u8>> {
    let e = |e: barcoders::error::Error| anyhow!("barcode: {e}");
    Ok(match sym {
        "code128" | "128" => {
            // Code128 requires a leading character-set selector; prepend the
            // requested set (default B = full ASCII) when the caller omitted one.
            let needs_prefix = !data.starts_with(['\u{00C0}', '\u{0181}', '\u{0106}']);
            let payload = if needs_prefix {
                format!("{code128_set}{data}")
            } else {
                data.to_string()
            };
            Code128::new(payload).map_err(e)?.encode()
        }
        "code39" | "39" => Code39::new(data).map_err(e)?.encode(),
        "code93" | "93" => Code93::new(data).map_err(e)?.encode(),
        "code11" | "11" => Code11::new(data).map_err(e)?.encode(),
        "codabar" => Codabar::new(data).map_err(e)?.encode(),
        "ean13" | "ean" => EAN13::new(data).map_err(e)?.encode(),
        "ean8" => EAN8::new(data).map_err(e)?.encode(),
        "upca" | "upc" | "upc-a" => EAN13::new(format!("0{data}")).map_err(e)?.encode(),
        "itf" | "interleaved" | "i2of5" => TF::interleaved(data).map_err(e)?.encode(),
        "std2of5" | "2of5" | "tf" => TF::standard(data).map_err(e)?.encode(),
        other => return Err(anyhow!("unknown symbology: {other}")),
    })
}

/// 1D barcode -> image handle.
///
/// opts: `data` (required string), `symbology`/`type` (default "code128"),
/// `scale` (px per narrow bar, default 2), `height` (bar height px, default 80),
/// `quiet` (quiet-zone px, default scale*10), `fg`/`bg` (default black on
/// white), `set` ("A"|"B"|"C", Code128 character set, default "B").
fn op_barcode_1d(opts: Value) -> Result<Value> {
    let data = req_str(&opts, "data")?;
    let sym = opts
        .get("symbology")
        .or_else(|| opts.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("code128")
        .to_ascii_lowercase();
    let scale = opt_u32(&opts, "scale", 2).max(1);
    let height = opt_u32(&opts, "height", 80).max(1);
    let quiet = opt_u32(&opts, "quiet", scale * 10);
    let fg = parse_color(opts.get("fg"));
    let bg = parse_color(opts.get("bg").or_else(|| opts.get("background")));
    let set = opts
        .get("set")
        .and_then(Value::as_str)
        .and_then(|s| s.chars().next())
        .map(|c| match c.to_ascii_uppercase() {
            'A' => '\u{00C0}',
            'C' => '\u{0106}',
            _ => '\u{0181}',
        })
        .unwrap_or('\u{0181}');

    let bars = encode_1d(&sym, data, set)?;
    if bars.is_empty() {
        return Err(anyhow!("barcode: empty encoding"));
    }
    let w = bars.len() as u32 * scale + 2 * quiet;
    let mut img = image::RgbaImage::from_pixel(w, height, bg);
    for (i, b) in bars.iter().enumerate() {
        if *b == 1 {
            let x0 = quiet + i as u32 * scale;
            for dx in 0..scale {
                for y in 0..height {
                    img.put_pixel(x0 + dx, y, fg);
                }
            }
        }
    }
    let handle = insert_image(image::DynamicImage::ImageRgba8(img));
    Ok(json!({"handle": handle, "width": w, "height": height, "symbology": sym, "bars": bars.len()}))
}

/// Generate a barcode straight to an image file. opts: output (path; format by
/// extension), kind => "qr" (default) | "1d", data, plus the matching generator
/// options (ec/scale/quiet for qr; symbology/scale/height for 1d; fg/bg both).
/// Returns `{ ok, path, kind, width, height }`.
fn op_barcode_save(opts: Value) -> Result<Value> {
    let output = req_str(&opts, "output")?.to_string();
    let kind = opts.get("kind").and_then(Value::as_str).unwrap_or("qr");
    let res = if kind == "1d" {
        op_barcode_1d(opts.clone())?
    } else {
        op_barcode_qr(opts.clone())?
    };
    let handle = res
        .get("handle")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("barcode produced no image"))?;
    op_img_save(json!({ "handle": handle, "path": output }))?;
    let _ = op_img_close(json!({ "handle": handle }));
    Ok(json!({
        "ok": true,
        "path": output,
        "kind": kind,
        "width": res.get("width").cloned().unwrap_or(json!(0)),
        "height": res.get("height").cloned().unwrap_or(json!(0)),
    }))
}

/// Batch-generate one barcode/QR image per value of a sheet column (label / asset
/// tag sheets). opts: path (sheet), column => name/index (required), dir =>
/// output directory (required), kind => "qr" (default) | "1d", symbology (for 1d),
/// ext => image extension (default "png"), prefix => filename stem (default ""),
/// plus any barcode style opts (scale, ec, fg, bg, height, …) passed through per
/// image, sheet, header. Files are `{dir}/{prefix}{value}.{ext}` (value
/// sanitized). Blank cells are skipped. Returns `{ count, files }`.
fn op_barcode_sheet(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let dir = req_str(&opts, "dir")?.to_string();
    let kind = opts.get("kind").and_then(Value::as_str).unwrap_or("qr").to_string();
    let ext = opts.get("ext").and_then(Value::as_str).unwrap_or("png").to_string();
    let prefix = opts.get("prefix").and_then(Value::as_str).unwrap_or("").to_string();
    std::fs::create_dir_all(&dir)?;

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

    let sanitize = |s: &str| -> String {
        let c: String = s
            .chars()
            .map(|ch| if ch.is_alphanumeric() { ch } else { '_' })
            .collect();
        if c.is_empty() {
            "blank".to_string()
        } else {
            c
        }
    };

    let data_start = if header && !rows.is_empty() { 1 } else { 0 };
    let mut files: Vec<String> = Vec::new();
    for row in &rows[data_start..] {
        let cell = row.as_array().and_then(|a| a.get(col)).cloned().unwrap_or(Value::Null);
        if sheet_cell_blank(&cell) {
            continue;
        }
        let value = cell_to_string(&cell);
        let out = format!("{dir}/{prefix}{}.{ext}", sanitize(&value));
        // Per-image opts: inherit caller styling, set this value/output/kind.
        let mut bopts = opts.clone();
        bopts["data"] = json!(value);
        bopts["output"] = json!(out);
        bopts["kind"] = json!(kind);
        op_barcode_save(bopts)?;
        files.push(out);
    }
    Ok(json!({ "count": files.len(), "files": files }))
}
