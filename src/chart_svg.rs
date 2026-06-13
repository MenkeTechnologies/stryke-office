// Vector chart rendering — the same chart model as chart_render, emitted as
// SVG markup instead of a raster image. Gives true vector output (scales
// losslessly) for line/scatter/pie/donut/bar/column/stacked/area/histogram/
// sankey. `chart_save` dispatches on the path extension: .svg -> this vector
// path, .pdf -> the chart embedded in a one-page PDF, any raster extension
// (.png/.jpg/.tif/.bmp/.webp/.gif) -> chart_render + img_save.

use std::fmt::Write as _;

fn svg_color(c: Rgba<u8>) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0[0], c.0[1], c.0[2])
}

fn svg_palette(i: usize) -> String {
    svg_color(palette(i))
}

/// Build an SVG document string for a chart spec.
fn chart_to_svg(opts: &Value) -> Result<String> {
    let kind = opts.get("type").and_then(Value::as_str).unwrap_or("bar");
    let w = opts.get("width").and_then(Value::as_u64).unwrap_or(800).max(120) as f64;
    let h = opts.get("height").and_then(Value::as_u64).unwrap_or(600).max(120) as f64;
    let empty_series: Vec<Value> = Vec::new();
    let series = opts.get("series").and_then(Value::as_array).unwrap_or(&empty_series);
    let cats: Vec<String> = opts
        .get("categories")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(cell_to_string).collect())
        .unwrap_or_default();
    let title = opts.get("title").and_then(Value::as_str).unwrap_or("");

    let mut s = String::new();
    let _ = write!(
        s,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="sans-serif">"#
    );
    let _ = write!(s, r##"<rect width="{w}" height="{h}" fill="#ffffff"/>"##);
    if !title.is_empty() {
        let _ = write!(
            s,
            r##"<text x="{}" y="26" text-anchor="middle" font-size="20" font-weight="bold" fill="#1e1e1e">{}</text>"##,
            w / 2.0,
            xml_escape(title)
        );
    }

    if kind == "sankey" {
        svg_sankey(&mut s, opts, w, h);
        s.push_str("</svg>");
        return Ok(s);
    }
    if opts.get("series").and_then(Value::as_array).is_none() {
        return Err(anyhow!("missing series (expected array)"));
    }
    if kind == "pie" || kind == "donut" || kind == "doughnut" {
        svg_pie(&mut s, series, w, h, kind != "pie");
        s.push_str("</svg>");
        return Ok(s);
    }

    let (l, r, t, b) = (60.0, w - 24.0, 44.0, h - 40.0);
    let pw = (r - l).max(1.0);
    let ph = (b - t).max(1.0);
    let scatter = kind == "scatter";

    let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut xmin, mut xmax) = (f64::INFINITY, f64::NEG_INFINITY);
    if scatter {
        for ser in series {
            for (x, y) in series_points(ser) {
                ymin = ymin.min(y);
                ymax = ymax.max(y);
                xmin = xmin.min(x);
                xmax = xmax.max(x);
            }
        }
    } else {
        for ser in series {
            for v in series_nums(ser) {
                ymin = ymin.min(v);
                ymax = ymax.max(v);
            }
        }
        ymin = ymin.min(0.0);
    }
    if !ymin.is_finite() || !ymax.is_finite() || (ymax - ymin).abs() < f64::EPSILON {
        ymax = ymin + 1.0;
    }
    if scatter && (!xmin.is_finite() || (xmax - xmin).abs() < f64::EPSILON) {
        xmax = xmin + 1.0;
    }
    let yp = |v: f64| b - (v - ymin) / (ymax - ymin) * ph;

    // axes + gridlines
    let _ = write!(s, r##"<line x1="{l}" y1="{b}" x2="{r}" y2="{b}" stroke="#1e1e1e"/>"##);
    let _ = write!(s, r##"<line x1="{l}" y1="{t}" x2="{l}" y2="{b}" stroke="#1e1e1e"/>"##);
    for i in 0..=5 {
        let v = ymin + (ymax - ymin) * (i as f64) / 5.0;
        let y = yp(v);
        let _ = write!(s, r##"<line x1="{l}" y1="{y}" x2="{r}" y2="{y}" stroke="#d2d2d2"/>"##);
        let _ = write!(s, r##"<text x="6" y="{}" font-size="11" fill="#1e1e1e">{v:.1}</text>"##, y + 4.0);
    }

    match kind {
        "line" | "area" => svg_line_area(&mut s, series, l, pw, &yp, b, kind == "area"),
        "scatter" => svg_scatter(&mut s, series, l, pw, xmin, xmax, &yp),
        "histogram" => svg_histogram(&mut s, series, opts, l, pw, t, b),
        "stacked" | "stacked_bar" => svg_bars(&mut s, series, &cats, l, pw, &yp, true),
        _ => svg_bars(&mut s, series, &cats, l, pw, &yp, false),
    }
    // category labels
    if !cats.is_empty() && !scatter {
        let slot = pw / cats.len().max(1) as f64;
        for (i, c) in cats.iter().enumerate() {
            let x = l + i as f64 * slot + slot * 0.3;
            let _ = write!(s, r##"<text x="{x}" y="{}" font-size="11" fill="#1e1e1e">{}</text>"##, b + 16.0, xml_escape(c));
        }
    }
    s.push_str("</svg>");
    Ok(s)
}

fn svg_bars(s: &mut String, series: &[Value], cats: &[String], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64, stacked: bool) {
    let ncat = series.iter().map(|x| series_nums(x).len()).max().unwrap_or(0).max(cats.len());
    if ncat == 0 {
        return;
    }
    let nser = series.len().max(1);
    let slot = pw / ncat as f64;
    if stacked {
        let mut cum = vec![0.0f64; ncat];
        let bw = slot * 0.8;
        for (si, ser) in series.iter().enumerate() {
            let col = svg_palette(si);
            for (ci, v) in series_nums(ser).into_iter().enumerate() {
                let x = l + ci as f64 * slot + slot * 0.1;
                let y0 = yp(cum[ci] + v);
                let height = (yp(cum[ci]) - y0).max(0.0);
                let _ = write!(s, r##"<rect x="{x}" y="{y0}" width="{bw}" height="{height}" fill="{col}"/>"##);
                cum[ci] += v;
            }
        }
    } else {
        let bw = slot * 0.8 / nser as f64;
        let base = yp(0.0);
        for (si, ser) in series.iter().enumerate() {
            let col = svg_palette(si);
            for (ci, v) in series_nums(ser).into_iter().enumerate() {
                let x = l + ci as f64 * slot + slot * 0.1 + si as f64 * bw;
                let top = yp(v);
                let (y, height) = if top < base { (top, base - top) } else { (base, top - base) };
                let _ = write!(s, r##"<rect x="{x}" y="{y}" width="{bw}" height="{}" fill="{col}"/>"##, height.max(0.0));
            }
        }
    }
}

fn svg_line_area(s: &mut String, series: &[Value], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64, b: f64, fill: bool) {
    for (si, ser) in series.iter().enumerate() {
        let col = svg_palette(si);
        let data = series_nums(ser);
        if data.is_empty() {
            continue;
        }
        let n = data.len();
        let xat = |i: usize| l + if n > 1 { i as f64 / (n - 1) as f64 * pw } else { pw / 2.0 };
        let pts: String = data.iter().enumerate().map(|(i, v)| format!("{:.1},{:.1} ", xat(i), yp(*v))).collect();
        if fill {
            let _ = write!(s, r##"<polygon points="{:.1},{:.1} {pts}{:.1},{:.1}" fill="{col}" fill-opacity="0.45"/>"##, xat(0), b, xat(n - 1), b);
        }
        let _ = write!(s, r##"<polyline points="{pts}" fill="none" stroke="{col}" stroke-width="2"/>"##);
    }
}

fn svg_scatter(s: &mut String, series: &[Value], l: f64, pw: f64, xmin: f64, xmax: f64, yp: &dyn Fn(f64) -> f64) {
    for (si, ser) in series.iter().enumerate() {
        let col = svg_palette(si);
        for (x, y) in series_points(ser) {
            let px = l + (x - xmin) / (xmax - xmin) * pw;
            let _ = write!(s, r##"<circle cx="{px:.1}" cy="{:.1}" r="4" fill="{col}"/>"##, yp(y));
        }
    }
}

fn svg_histogram(s: &mut String, series: &[Value], opts: &Value, l: f64, pw: f64, t: f64, b: f64) {
    let data = series.first().map(series_nums).unwrap_or_default();
    if data.is_empty() {
        return;
    }
    let nbins = opts.get("bins").and_then(Value::as_u64).unwrap_or(10).clamp(1, 200) as usize;
    let (lo, hi) = data.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, c), &v| (a.min(v), c.max(v)));
    let span = if (hi - lo).abs() < f64::EPSILON { 1.0 } else { hi - lo };
    let mut counts = vec![0u32; nbins];
    for &v in &data {
        let mut idx = ((v - lo) / span * nbins as f64) as usize;
        if idx >= nbins {
            idx = nbins - 1;
        }
        counts[idx] += 1;
    }
    let maxc = *counts.iter().max().unwrap_or(&1) as f64;
    let ph = (b - t).max(1.0);
    let slot = pw / nbins as f64;
    let col = svg_palette(0);
    for (i, &c) in counts.iter().enumerate() {
        let x = l + i as f64 * slot + slot * 0.05;
        let bh = c as f64 / maxc * ph;
        let _ = write!(s, r##"<rect x="{x}" y="{}" width="{}" height="{bh}" fill="{col}"/>"##, b - bh, slot * 0.9);
    }
}

fn svg_pie(s: &mut String, series: &[Value], w: f64, h: f64, donut: bool) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let total: f64 = data.iter().sum();
    if total <= 0.0 {
        return;
    }
    let (cx, cy) = (w / 2.0, h / 2.0 + 10.0);
    let radius = (w.min(h) / 2.0 - 40.0).max(20.0);
    let mut angle = -std::f64::consts::FRAC_PI_2;
    for (i, v) in data.iter().enumerate() {
        let sweep = v / total * std::f64::consts::TAU;
        let (x0, y0) = (cx + radius * angle.cos(), cy + radius * angle.sin());
        let a1 = angle + sweep;
        let (x1, y1) = (cx + radius * a1.cos(), cy + radius * a1.sin());
        let large = if sweep > std::f64::consts::PI { 1 } else { 0 };
        let _ = write!(
            s,
            r##"<path d="M {cx:.1},{cy:.1} L {x0:.1},{y0:.1} A {radius:.1},{radius:.1} 0 {large},1 {x1:.1},{y1:.1} Z" fill="{}"/>"##,
            svg_palette(i)
        );
        angle = a1;
    }
    if donut {
        let _ = write!(s, r##"<circle cx="{cx:.1}" cy="{cy:.1}" r="{:.1}" fill="#ffffff"/>"##, radius * 0.55);
    }
}

/// Sankey: nodes `[{name}]`, links `[{source, target, value}]` (indices into
/// nodes). A simple two-stage layout — distinct source nodes on the left,
/// distinct targets on the right — with band widths proportional to value.
fn svg_sankey(s: &mut String, opts: &Value, w: f64, h: f64) {
    let nodes: Vec<String> = opts
        .get("nodes")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|n| n.get("name").and_then(Value::as_str).map(String::from).unwrap_or_default()).collect())
        .unwrap_or_default();
    let links = opts.get("links").and_then(Value::as_array).cloned().unwrap_or_default();
    if links.is_empty() {
        return;
    }
    let val = |lk: &Value| lk.get("value").and_then(Value::as_f64).unwrap_or(0.0);
    let src = |lk: &Value| lk.get("source").and_then(Value::as_u64).unwrap_or(0) as usize;
    let tgt = |lk: &Value| lk.get("target").and_then(Value::as_u64).unwrap_or(0) as usize;

    let mut sources: Vec<usize> = links.iter().map(&src).collect();
    sources.sort_unstable();
    sources.dedup();
    let mut targets: Vec<usize> = links.iter().map(&tgt).collect();
    targets.sort_unstable();
    targets.dedup();
    let total: f64 = links.iter().map(&val).sum();
    if total <= 0.0 {
        return;
    }
    let (lx, rx) = (80.0, w - 100.0);
    let (top, bot) = (50.0, h - 30.0);
    let avail = bot - top;
    let nodew = 16.0;

    // y positions of each source/target node stacked by total throughput.
    let band = |ids: &[usize], side_src: bool| -> std::collections::HashMap<usize, (f64, f64)> {
        let mut pos = std::collections::HashMap::new();
        let mut y = top;
        for &id in ids {
            let sum: f64 = links
                .iter()
                .filter(|lk| if side_src { src(lk) == id } else { tgt(lk) == id })
                .map(&val)
                .sum();
            let height = sum / total * avail;
            pos.insert(id, (y, height));
            y += height + 8.0;
        }
        pos
    };
    let spos = band(&sources, true);
    let tpos = band(&targets, false);

    // node rectangles + labels
    for (&id, &(y, ht)) in spos.iter() {
        let _ = write!(s, r##"<rect x="{lx}" y="{y:.1}" width="{nodew}" height="{ht:.1}" fill="#4472c4"/>"##);
        if let Some(name) = nodes.get(id) {
            let _ = write!(s, r##"<text x="{}" y="{:.1}" font-size="11" text-anchor="end" fill="#1e1e1e">{}</text>"##, lx - 4.0, y + ht / 2.0, xml_escape(name));
        }
    }
    for (&id, &(y, ht)) in tpos.iter() {
        let _ = write!(s, r##"<rect x="{:.1}" y="{y:.1}" width="{nodew}" height="{ht:.1}" fill="#ed7d31"/>"##, rx);
        if let Some(name) = nodes.get(id) {
            let _ = write!(s, r##"<text x="{:.1}" y="{:.1}" font-size="11" fill="#1e1e1e">{}</text>"##, rx + nodew + 4.0, y + ht / 2.0, xml_escape(name));
        }
    }

    // flow bands as cubic Bezier ribbons
    let mut soff: std::collections::HashMap<usize, f64> = std::collections::HashMap::new();
    let mut toff: std::collections::HashMap<usize, f64> = std::collections::HashMap::new();
    for (i, lk) in links.iter().enumerate() {
        let v = val(lk);
        let bh = v / total * avail;
        let (sy0, _) = *spos.get(&src(lk)).unwrap_or(&(top, 0.0));
        let (ty0, _) = *tpos.get(&tgt(lk)).unwrap_or(&(top, 0.0));
        let so = soff.entry(src(lk)).or_insert(0.0);
        let sy = sy0 + *so;
        *so += bh;
        let to = toff.entry(tgt(lk)).or_insert(0.0);
        let ty = ty0 + *to;
        *to += bh;
        let x0 = lx + nodew;
        let xc = (x0 + rx) / 2.0;
        let _ = write!(
            s,
            r##"<path d="M {x0:.1},{sy:.1} C {xc:.1},{sy:.1} {xc:.1},{ty:.1} {rx:.1},{ty:.1} L {rx:.1},{:.1} C {xc:.1},{:.1} {xc:.1},{:.1} {x0:.1},{:.1} Z" fill="{}" fill-opacity="0.45"/>"##,
            ty + bh, ty + bh, sy + bh, sy + bh, svg_palette(i)
        );
    }
}

fn op_chart_svg(opts: Value) -> Result<Value> {
    let svg = chart_to_svg(&opts)?;
    if let Some(path) = opts.get("path").and_then(Value::as_str) {
        std::fs::write(path, &svg)?;
        return Ok(json!({"ok": true, "path": path, "bytes": svg.len()}));
    }
    Ok(json!({"svg": svg}))
}

/// Render a chart to ANY format, chosen by the path extension:
///   .svg            -> vector SVG
///   .pdf            -> chart embedded (as JPEG) in a one-page PDF
///   .png/.jpg/.jpeg/.tif/.tiff/.bmp/.webp/.gif -> raster via chart_render
fn op_chart_save(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?.to_string();
    let ext = ext_of(&path);
    if ext == "svg" {
        let svg = chart_to_svg(&opts)?;
        std::fs::write(&path, &svg)?;
        return Ok(json!({"ok": true, "path": path, "format": "svg"}));
    }
    // Raster the chart to an image handle, then either save directly or embed
    // into a PDF.
    let rendered = op_chart_render(opts.clone())?;
    let handle = rendered
        .get("handle")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("chart render produced no handle"))?;
    let result = (|| {
        if ext == "pdf" {
            let jpeg = with_image(handle, |img| {
                let mut buf = std::io::Cursor::new(Vec::new());
                img.to_rgb8()
                    .write_to(&mut buf, image::ImageFormat::Jpeg)
                    .map_err(|e| anyhow!("encode jpeg: {e}"))?;
                Ok(buf.into_inner())
            })?;
            let (w, h) = with_image(handle, |img| {
                use image::GenericImageView;
                Ok(img.dimensions())
            })?;
            let pdf = pdf_with_jpeg(&jpeg, w, h);
            std::fs::write(&path, pdf)?;
            Ok(json!({"ok": true, "path": path, "format": "pdf"}))
        } else {
            with_image(handle, |img| {
                img.save(&path).map_err(|e| anyhow!("save {path}: {e}"))?;
                Ok(json!({"ok": true, "path": path, "format": ext}))
            })
        }
    })();
    images().lock().remove(&handle);
    result
}

/// Minimal single-page PDF embedding a JPEG (DCTDecode) at full page size.
fn pdf_with_jpeg(jpeg: &[u8], iw: u32, ih: u32) -> Vec<u8> {
    // A4-ish page sized to the image aspect at 72dpi-ish scaling.
    let pw = 595.0;
    let ph = pw * ih as f64 / iw.max(1) as f64;
    let mut objs: Vec<Vec<u8>> = Vec::new();
    objs.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    objs.push(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec());
    objs.push(
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw:.0} {ph:.0}] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>")
            .into_bytes(),
    );
    let mut im = format!(
        "<< /Type /XObject /Subtype /Image /Width {iw} /Height {ih} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
        jpeg.len()
    )
    .into_bytes();
    im.extend_from_slice(jpeg);
    im.extend_from_slice(b"\nendstream");
    objs.push(im);
    let content = format!("q {pw:.0} 0 0 {ph:.0} 0 0 cm /Im0 Do Q");
    objs.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()).into_bytes());

    let mut out = b"%PDF-1.5\n".to_vec();
    let mut offsets = Vec::new();
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
