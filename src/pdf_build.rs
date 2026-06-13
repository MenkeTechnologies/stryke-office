// Multi-element PDF document builder.
//
// `office__pdf_build` lays out a paginated PDF from a list of elements —
// flowing headings/paragraphs, absolutely-placed text, embedded images
// (DCTDecode JPEG, from a file path or an image handle), and vector
// rectangles/lines. It uses the PDF base-14 standard fonts (Helvetica /
// Times / Courier), so nothing is embedded and any viewer renders it. This is
// the structured-document counterpart to the text-only `pdf_write` and the
// single-chart `chart_save .pdf`. Coordinates are top-left origin (y down),
// converted internally to PDF's bottom-left space.

/// Escape a string for a PDF literal `( … )`.
fn pdf_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 128 => out.push(c),
            _ => out.push('?'), // base-14 fonts are Latin-1; keep ASCII only
        }
    }
    out
}

/// Standard-font resource id for a name: helvetica(+bold), times, courier.
fn pdf_font_id(name: &str) -> &'static str {
    match name {
        "bold" | "helvetica-bold" => "F2",
        "times" | "serif" => "F3",
        "courier" | "mono" => "F4",
        _ => "F1",
    }
}

/// RGB 0..1 components of a color value ("#rrggbb" / [r,g,b]); default black.
fn pdf_rgb(v: Option<&Value>) -> (f64, f64, f64) {
    let c = parse_color(v);
    (c.0[0] as f64 / 255.0, c.0[1] as f64 / 255.0, c.0[2] as f64 / 255.0)
}

/// Rough Helvetica advance width per character (~0.5em) for word wrapping.
fn wrap_text(text: &str, size: f64, max_w: f64) -> Vec<String> {
    let char_w = size * 0.5;
    let max_chars = (max_w / char_w).floor().max(1.0) as usize;
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            if line.is_empty() {
                line = word.to_string();
            } else if line.len() + 1 + word.len() <= max_chars {
                line.push(' ');
                line.push_str(word);
            } else {
                lines.push(std::mem::take(&mut line));
                line = word.to_string();
            }
        }
        lines.push(line);
    }
    lines
}

/// A JPEG encoding (bytes + dims) of an element's image, from `handle` or
/// `path`.
fn pdf_element_jpeg(el: &Value) -> Result<(Vec<u8>, u32, u32)> {
    let dynimg = if let Some(h) = el.get("handle").and_then(Value::as_u64) {
        DynamicImage::ImageRgba8(rgba_of(h)?)
    } else if let Some(p) = el.get("path").and_then(Value::as_str) {
        image::open(p).map_err(|e| anyhow!("image {p}: {e}"))?
    } else {
        return Err(anyhow!("pdf image element needs path or handle"));
    };
    let (w, h) = (dynimg.width(), dynimg.height());
    let mut buf = Cursor::new(Vec::new());
    dynimg
        .to_rgb8()
        .write_to(&mut buf, image::ImageFormat::Jpeg)
        .map_err(|e| anyhow!("encode jpeg: {e}"))?;
    Ok((buf.into_inner(), w, h))
}

struct PdfPage {
    content: String,
    images: Vec<(String, Vec<u8>, u32, u32)>,
}

impl PdfPage {
    fn new() -> Self {
        PdfPage { content: String::new(), images: Vec::new() }
    }
}

fn op_pdf_build(opts: Value) -> Result<Value> {
    use std::fmt::Write as _;
    let path = req_str(&opts, "path")?.to_string();
    let (pw, ph) = match opts.get("page_size").and_then(Value::as_array) {
        Some(a) if a.len() >= 2 => (
            a[0].as_f64().unwrap_or(595.0),
            a[1].as_f64().unwrap_or(842.0),
        ),
        _ => (595.0, 842.0), // A4 in points
    };
    let margin = opts.get("margin").and_then(Value::as_f64).unwrap_or(50.0);
    let elements = opts
        .get("elements")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing elements (expected array)"))?;

    let mut pages = vec![PdfPage::new()];
    let mut ty = margin; // distance from the top of the current page
    let content_w = pw - 2.0 * margin;

    // Ensure room for `need` points of vertical space, else start a new page.
    let ensure = |pages: &mut Vec<PdfPage>, ty: &mut f64, need: f64| {
        if *ty + need > ph - margin {
            pages.push(PdfPage::new());
            *ty = margin;
        }
    };

    for el in elements {
        let kind = el.get("type").and_then(Value::as_str).unwrap_or("paragraph");
        match kind {
            "pagebreak" => {
                pages.push(PdfPage::new());
                ty = margin;
            }
            "heading" => {
                let level = el.get("level").and_then(Value::as_u64).unwrap_or(1).clamp(1, 6);
                let size = (24 - (level as i64 - 1) * 3).max(11) as f64;
                let lh = size * 1.5;
                ensure(&mut pages, &mut ty, lh);
                let (r, g, b) = pdf_rgb(el.get("color"));
                let text = el.get("text").and_then(Value::as_str).unwrap_or("");
                let pdf_y = ph - ty - size;
                let p = pages.last_mut().unwrap();
                let _ = write!(p.content, "BT /F2 {size} Tf {r:.3} {g:.3} {b:.3} rg {margin} {pdf_y:.1} Td ({}) Tj ET\n", pdf_escape(text));
                ty += lh;
            }
            "paragraph" => {
                let size = el.get("size").and_then(Value::as_f64).unwrap_or(11.0);
                let lh = size * 1.35;
                let font = pdf_font_id(el.get("font").and_then(Value::as_str).unwrap_or("helvetica"));
                let (r, g, b) = pdf_rgb(el.get("color"));
                let text = el.get("text").and_then(Value::as_str).unwrap_or("");
                for line in wrap_text(text, size, content_w) {
                    ensure(&mut pages, &mut ty, lh);
                    let pdf_y = ph - ty - size;
                    let p = pages.last_mut().unwrap();
                    let _ = write!(p.content, "BT /{font} {size} Tf {r:.3} {g:.3} {b:.3} rg {margin} {pdf_y:.1} Td ({}) Tj ET\n", pdf_escape(&line));
                    ty += lh;
                }
                ty += lh * 0.4; // paragraph spacing
            }
            "text" => {
                // Absolutely placed (top-left origin).
                let x = el.get("x").and_then(Value::as_f64).unwrap_or(margin);
                let y = el.get("y").and_then(Value::as_f64).unwrap_or(ty);
                let size = el.get("size").and_then(Value::as_f64).unwrap_or(12.0);
                let font = pdf_font_id(el.get("font").and_then(Value::as_str).unwrap_or("helvetica"));
                let (r, g, b) = pdf_rgb(el.get("color"));
                let text = el.get("text").and_then(Value::as_str).unwrap_or("");
                let pdf_y = ph - y - size;
                let p = pages.last_mut().unwrap();
                let _ = write!(p.content, "BT /{font} {size} Tf {r:.3} {g:.3} {b:.3} rg {x:.1} {pdf_y:.1} Td ({}) Tj ET\n", pdf_escape(text));
            }
            "rect" => {
                let x = el.get("x").and_then(Value::as_f64).unwrap_or(margin);
                let y = el.get("y").and_then(Value::as_f64).unwrap_or(ty);
                let w = el.get("width").and_then(Value::as_f64).unwrap_or(100.0);
                let hh = el.get("height").and_then(Value::as_f64).unwrap_or(40.0);
                let fill = el.get("fill").and_then(Value::as_bool).unwrap_or(true);
                let (r, g, b) = pdf_rgb(el.get("color"));
                let pdf_y = ph - y - hh;
                let p = pages.last_mut().unwrap();
                if fill {
                    let _ = write!(p.content, "{r:.3} {g:.3} {b:.3} rg {x:.1} {pdf_y:.1} {w:.1} {hh:.1} re f\n");
                } else {
                    let _ = write!(p.content, "{r:.3} {g:.3} {b:.3} RG {x:.1} {pdf_y:.1} {w:.1} {hh:.1} re S\n");
                }
            }
            "line" => {
                let x0 = el.get("x0").and_then(Value::as_f64).unwrap_or(margin);
                let y0 = el.get("y0").and_then(Value::as_f64).unwrap_or(ty);
                let x1 = el.get("x1").and_then(Value::as_f64).unwrap_or(margin);
                let y1 = el.get("y1").and_then(Value::as_f64).unwrap_or(ty);
                let (r, g, b) = pdf_rgb(el.get("color"));
                let p = pages.last_mut().unwrap();
                let _ = write!(p.content, "{r:.3} {g:.3} {b:.3} RG {x0:.1} {:.1} m {x1:.1} {:.1} l S\n", ph - y0, ph - y1);
            }
            "image" => {
                let (jpeg, iw, ih) = pdf_element_jpeg(el)?;
                let x = el.get("x").and_then(Value::as_f64).unwrap_or(margin);
                let w = el.get("width").and_then(Value::as_f64).unwrap_or(iw as f64);
                let hh = el.get("height").and_then(Value::as_f64).unwrap_or(ih as f64 * w / iw.max(1) as f64);
                // flowing image when no explicit y
                let y = match el.get("y").and_then(Value::as_f64) {
                    Some(y) => y,
                    None => {
                        ensure(&mut pages, &mut ty, hh + 6.0);
                        let yy = ty;
                        ty += hh + 6.0;
                        yy
                    }
                };
                let pdf_y = ph - y - hh;
                let p = pages.last_mut().unwrap();
                let name = format!("Im{}", p.images.len());
                let _ = write!(p.content, "q {w:.1} 0 0 {hh:.1} {x:.1} {pdf_y:.1} cm /{name} Do Q\n");
                p.images.push((name, jpeg, iw, ih));
            }
            other => return Err(anyhow!("unknown pdf element type: {other}")),
        }
    }

    let bytes = assemble_pdf(&pages, pw, ph);
    std::fs::write(&path, &bytes)?;
    Ok(json!({"ok": true, "path": path, "pages": pages.len(), "bytes": bytes.len()}))
}

/// Serialize the accumulated pages into a single PDF byte buffer.
fn assemble_pdf(pages: &[PdfPage], pw: f64, ph: f64) -> Vec<u8> {
    // Object 1 = Catalog, 2 = Pages; the rest are emitted per page.
    let mut objs: Vec<Vec<u8>> = vec![Vec::new(), Vec::new()];
    let mut page_refs: Vec<usize> = Vec::new();
    let fonts = "/Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> /F2 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >> /F3 << /Type /Font /Subtype /Type1 /BaseFont /Times-Roman >> /F4 << /Type /Font /Subtype /Type1 /BaseFont /Courier >> >>";

    for page in pages {
        // image XObjects first
        let mut xobjects = String::new();
        for (name, jpeg, iw, ih) in &page.images {
            let mut obj = format!(
                "<< /Type /XObject /Subtype /Image /Width {iw} /Height {ih} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
                jpeg.len()
            )
            .into_bytes();
            obj.extend_from_slice(jpeg);
            obj.extend_from_slice(b"\nendstream");
            objs.push(obj);
            xobjects.push_str(&format!("/{name} {} 0 R ", objs.len()));
        }
        // content stream
        let content = page.content.as_bytes();
        objs.push(format!("<< /Length {} >>\nstream\n", content.len()).into_bytes());
        let ci = objs.len() - 1;
        objs[ci].extend_from_slice(content);
        objs[ci].extend_from_slice(b"\nendstream");
        let content_num = ci + 1;
        // page object
        let resources = if xobjects.is_empty() {
            format!("<< {fonts} >>")
        } else {
            format!("<< {fonts} /XObject << {xobjects}>> >>")
        };
        objs.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw:.0} {ph:.0}] /Resources {resources} /Contents {content_num} 0 R >>"
            )
            .into_bytes(),
        );
        page_refs.push(objs.len());
    }

    // catalog + pages
    objs[0] = b"<< /Type /Catalog /Pages 2 0 R >>".to_vec();
    let kids: String = page_refs.iter().map(|n| format!("{n} 0 R ")).collect();
    objs[1] = format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", page_refs.len()).into_bytes();

    // write body + xref
    let mut out = b"%PDF-1.5\n".to_vec();
    let mut offsets = Vec::with_capacity(objs.len());
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF", objs.len() + 1).as_bytes(),
    );
    out
}
