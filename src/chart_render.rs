// Standalone chart rendering — data in, an image handle out.
//
// `office__chart_render` rasterizes a chart (bar/column/line/area/scatter/pie)
// onto a fresh RGBA image using imageproc + the vendored font, then stores it
// in the same image registry the PIL surface uses and returns its handle. The
// caller then `img_save`s it to ANY raster format (png/jpeg/gif/bmp/webp/tiff)
// or further processes it — so "render a chart in whatever format I want" is
// just chart_render + img_save. Fully self-contained: no plotters, no system
// fonts, no external binaries.

use ab_glyph::{FontRef, PxScale};
use image::{Rgba, RgbaImage};
use imageproc::drawing::{
    draw_filled_circle_mut, draw_filled_rect_mut, draw_line_segment_mut, draw_polygon_mut,
    draw_text_mut,
};
use imageproc::point::Point;
use imageproc::rect::Rect;

const PALETTE: &[&str] = &[
    "#4472C4", "#ED7D31", "#A5A5A5", "#FFC000", "#5B9BD5", "#70AD47", "#264478", "#9E480E",
    "#636363", "#997300",
];

fn palette(i: usize) -> Rgba<u8> {
    parse_color(Some(&Value::String(PALETTE[i % PALETTE.len()].to_string())))
}

fn font() -> FontRef<'static> {
    FontRef::try_from_slice(FONT_BYTES).expect("vendored font is valid")
}

/// Numeric data of a series (`data:[n,...]`), ignoring non-numbers.
fn series_nums(s: &Value) -> Vec<f64> {
    s.get("data")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default()
}

/// Bubble points of a series (`data:[[x,y,size],...]`).
fn series_points3(s: &Value) -> Vec<(f64, f64, f64)> {
    s.get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some((
                        p.first()?.as_f64()?,
                        p.get(1)?.as_f64()?,
                        p.get(2).and_then(Value::as_f64).unwrap_or(6.0),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Scatter points of a series (`data:[[x,y],...]`).
fn series_points(s: &Value) -> Vec<(f64, f64)> {
    s.get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some((p.first()?.as_f64()?, p.get(1)?.as_f64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn series_color(s: &Value, i: usize) -> Rgba<u8> {
    match s.get("color") {
        Some(c) => parse_color(Some(c)),
        None => palette(i),
    }
}

/// Conditional formatting + data validation declared on a sheet:
/// `conditional:[{range:[r1,c1,r2,c2], rule, value, value2?, format:{…}}]`
/// `validations:[{range:[r1,c1,r2,c2], list:[strings]}]`.
fn write_xlsx_cond_val(ws: &mut rust_xlsxwriter::Worksheet, s: &Value) -> Result<()> {
    use rust_xlsxwriter::{ConditionalFormatCell, ConditionalFormatCellRule, DataValidation};
    if let Some(conds) = s.get("conditional").and_then(Value::as_array) {
        for c in conds {
            let Some(rng) = quad(c, "range") else { continue };
            let v = c.get("value").and_then(Value::as_f64).unwrap_or(0.0);
            let v2 = c.get("value2").and_then(Value::as_f64).unwrap_or(0.0);
            let rule = match c.get("rule").and_then(Value::as_str).unwrap_or("greater_than") {
                "less_than" => ConditionalFormatCellRule::LessThan(v),
                "greater_equal" => ConditionalFormatCellRule::GreaterThanOrEqualTo(v),
                "less_equal" => ConditionalFormatCellRule::LessThanOrEqualTo(v),
                "equal" => ConditionalFormatCellRule::EqualTo(v),
                "not_equal" => ConditionalFormatCellRule::NotEqualTo(v),
                "between" => ConditionalFormatCellRule::Between(v, v2),
                "not_between" => ConditionalFormatCellRule::NotBetween(v, v2),
                _ => ConditionalFormatCellRule::GreaterThan(v),
            };
            let mut cf = ConditionalFormatCell::new().set_rule(rule);
            if let Some(fmt) = c.get("format").and_then(Value::as_object).and_then(xlsx_format) {
                cf = cf.set_format(fmt);
            }
            ws.add_conditional_format(rng[0], rng[1] as u16, rng[2], rng[3] as u16, &cf)?;
        }
    }
    if let Some(vals) = s.get("validations").and_then(Value::as_array) {
        for v in vals {
            let Some(rng) = quad(v, "range") else { continue };
            if let Some(list) = v.get("list").and_then(Value::as_array) {
                let items: Vec<String> = list.iter().map(cell_to_string).collect();
                let dv = DataValidation::new().allow_list_strings(&items)?;
                ws.add_data_validation(rng[0], rng[1] as u16, rng[2], rng[3] as u16, &dv)?;
            }
        }
    }
    Ok(())
}

fn op_chart_render(opts: Value) -> Result<Value> {
    let kind = opts.get("type").and_then(Value::as_str).unwrap_or("bar").to_string();
    let w = opts.get("width").and_then(Value::as_u64).unwrap_or(800).max(120) as u32;
    let h = opts.get("height").and_then(Value::as_u64).unwrap_or(600).max(120) as u32;
    let empty_series: Vec<Value> = Vec::new();
    let series = opts.get("series").and_then(Value::as_array).unwrap_or(&empty_series);
    let cats: Vec<String> = opts
        .get("categories")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(cell_to_string).collect())
        .unwrap_or_default();
    let title = opts.get("title").and_then(Value::as_str).unwrap_or("");

    let mut img = RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255]));
    let fnt = font();
    let black = Rgba([30, 30, 30, 255]);
    let grid = Rgba([210, 210, 210, 255]);

    // Title.
    if !title.is_empty() {
        draw_text_mut(&mut img, black, (w as i32) / 2 - title.len() as i32 * 5, 8, PxScale::from(22.0), &fnt, title);
    }

    let (l, r, t, b) = (60i32, w as i32 - 24, 44i32, h as i32 - 40);
    let pw = (r - l).max(1) as f64;
    let ph = (b - t).max(1) as f64;

    if kind == "sankey" {
        render_sankey(&mut img, &opts, w as f64, h as f64);
        let handle = insert_image(DynamicImage::ImageRgba8(img));
        return Ok(json!({"handle": handle, "width": w, "height": h, "type": kind}));
    }
    if opts.get("series").and_then(Value::as_array).is_none() {
        return Err(anyhow!("missing series (expected array)"));
    }
    if kind == "radar" {
        render_radar(&mut img, &fnt, series, &cats, l, t, r, b, black, grid);
        let handle = insert_image(DynamicImage::ImageRgba8(img));
        return Ok(json!({"handle": handle, "width": w, "height": h, "type": kind}));
    }
    if kind == "pie" || kind == "donut" || kind == "doughnut" {
        render_pie(&mut img, series, l, t, r, b, kind != "pie");
        let handle = insert_image(DynamicImage::ImageRgba8(img));
        return Ok(json!({"handle": handle, "width": w, "height": h, "type": kind}));
    }

    // Cartesian: axes + y scale.
    draw_line_segment_mut(&mut img, (l as f32, b as f32), (r as f32, b as f32), black); // x axis
    draw_line_segment_mut(&mut img, (l as f32, t as f32), (l as f32, b as f32), black); // y axis

    let scatter = kind == "scatter" || kind == "bubble";
    let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut xmin, mut xmax) = (f64::INFINITY, f64::NEG_INFINITY);
    if scatter {
        for s in series {
            for (x, y, _) in series_points3(s) {
                ymin = ymin.min(y);
                ymax = ymax.max(y);
                xmin = xmin.min(x);
                xmax = xmax.max(x);
            }
        }
    } else {
        for s in series {
            for v in series_nums(s) {
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
    let yp = |v: f64| (b as f64 - (v - ymin) / (ymax - ymin) * ph) as f32;

    // Y gridlines + tick labels (5 ticks).
    for i in 0..=5 {
        let v = ymin + (ymax - ymin) * (i as f64) / 5.0;
        let y = yp(v);
        draw_line_segment_mut(&mut img, (l as f32, y), (r as f32, y), grid);
        draw_text_mut(&mut img, black, 4, y as i32 - 6, PxScale::from(12.0), &fnt, &format!("{v:.1}"));
    }

    match kind.as_str() {
        "line" | "area" => render_line_area(&mut img, series, l, r, b, ymin, ymax, kind == "area"),
        "scatter" => render_scatter(&mut img, series, l as f64, pw, xmin, xmax, yp),
        "bubble" => render_bubble(&mut img, series, l as f64, pw, xmin, xmax, yp),
        "histogram" => render_histogram(&mut img, &fnt, series, &opts, l, pw, t, b, black),
        "stacked" | "stacked_bar" => {
            render_bars(&mut img, &fnt, series, &cats, l, pw, b, &yp, black, true)
        }
        _ => render_bars(&mut img, &fnt, series, &cats, l, pw, b, &yp, black, false),
    }

    let handle = insert_image(DynamicImage::ImageRgba8(img));
    Ok(json!({"handle": handle, "width": w, "height": h, "type": kind}))
}

#[allow(clippy::too_many_arguments)]
fn render_bars(
    img: &mut RgbaImage,
    fnt: &FontRef,
    series: &[Value],
    cats: &[String],
    l: i32,
    pw: f64,
    b: i32,
    yp: &dyn Fn(f64) -> f32,
    black: Rgba<u8>,
    stacked: bool,
) {
    let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0).max(cats.len());
    if ncat == 0 {
        return;
    }
    let nser = series.len().max(1);
    let slot = pw / ncat as f64;
    let base = yp(0.0);
    if stacked {
        let mut cum = vec![0.0f64; ncat];
        let barw = (slot * 0.8).max(1.0);
        for (si, s) in series.iter().enumerate() {
            let color = series_color(s, si);
            for (ci, v) in series_nums(s).into_iter().enumerate() {
                let x = l as f64 + ci as f64 * slot + slot * 0.1;
                let y0 = yp(cum[ci] + v);
                let y1 = yp(cum[ci]);
                let hgt = (y1 - y0).max(1.0) as u32;
                draw_filled_rect_mut(
                    img,
                    Rect::at(x as i32, y0 as i32).of_size(barw as u32, hgt),
                    color,
                );
                cum[ci] += v;
            }
        }
    } else {
        let barw = (slot * 0.8 / nser as f64).max(1.0);
        for (si, s) in series.iter().enumerate() {
            let color = series_color(s, si);
            for (ci, v) in series_nums(s).into_iter().enumerate() {
                let x = l as f64 + ci as f64 * slot + slot * 0.1 + si as f64 * barw;
                let top = yp(v);
                let (y0, y1) = if top < base { (top, base) } else { (base, top) };
                let hgt = (y1 - y0).max(1.0) as u32;
                draw_filled_rect_mut(
                    img,
                    Rect::at(x as i32, y0 as i32).of_size(barw.max(1.0) as u32, hgt),
                    color,
                );
            }
        }
    }
    for (ci, cat) in cats.iter().enumerate() {
        let x = l as f64 + ci as f64 * slot + slot * 0.25;
        draw_text_mut(img, black, x as i32, b + 6, PxScale::from(12.0), fnt, cat);
    }
}

/// Histogram of the first series' raw data, binned into `bins` (opt, default
/// 10) equal-width buckets; bars are bucket counts.
#[allow(clippy::too_many_arguments)]
fn render_histogram(
    img: &mut RgbaImage,
    fnt: &FontRef,
    series: &[Value],
    opts: &Value,
    l: i32,
    pw: f64,
    t: i32,
    b: i32,
    black: Rgba<u8>,
) {
    let data = series.first().map(series_nums).unwrap_or_default();
    if data.is_empty() {
        return;
    }
    let nbins = opts.get("bins").and_then(Value::as_u64).unwrap_or(10).clamp(1, 200) as usize;
    let (lo, hi) = data.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &v| {
        (a.min(v), b.max(v))
    });
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
    let ph = (b - t).max(1) as f64;
    let slot = pw / nbins as f64;
    let color = series_color(series.first().unwrap_or(&Value::Null), 0);
    for (i, &c) in counts.iter().enumerate() {
        let x = l as f64 + i as f64 * slot + slot * 0.05;
        let bh = c as f64 / maxc * ph;
        let y0 = b as f64 - bh;
        draw_filled_rect_mut(
            img,
            Rect::at(x as i32, y0 as i32).of_size((slot * 0.9).max(1.0) as u32, bh.max(1.0) as u32),
            color,
        );
    }
    draw_text_mut(img, black, l, b + 6, PxScale::from(12.0), fnt, &format!("{lo:.1}"));
    draw_text_mut(img, black, (l as f64 + pw - 30.0) as i32, b + 6, PxScale::from(12.0), fnt, &format!("{hi:.1}"));
}

#[allow(clippy::too_many_arguments)]
fn render_line_area(
    img: &mut RgbaImage,
    series: &[Value],
    l: i32,
    r: i32,
    b: i32,
    ymin: f64,
    ymax: f64,
    fill: bool,
) {
    let pw = (r - l).max(1) as f64;
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        let data = series_nums(s);
        if data.is_empty() {
            continue;
        }
        let n = data.len();
        let xat = |i: usize| {
            l as f64 + if n > 1 { i as f64 / (n - 1) as f64 * pw } else { pw / 2.0 }
        };
        let yv = |v: f64| (b as f64 - (v - ymin) / (ymax - ymin) * (b as f64 - 44.0).max(1.0)) as f32;
        if fill {
            let mut poly: Vec<Point<i32>> = Vec::with_capacity(n + 2);
            poly.push(Point::new(xat(0) as i32, b));
            for (i, v) in data.iter().enumerate() {
                poly.push(Point::new(xat(i) as i32, yv(*v) as i32));
            }
            poly.push(Point::new(xat(n - 1) as i32, b));
            // dedup consecutive identical points (draw_polygon_mut requirement)
            poly.dedup();
            if poly.len() >= 3 && poly.first() != poly.last() {
                let mut fillc = color;
                fillc.0[3] = 120;
                draw_polygon_mut(img, &poly, fillc);
            }
        }
        for i in 1..n {
            draw_line_segment_mut(
                img,
                (xat(i - 1) as f32, yv(data[i - 1])),
                (xat(i) as f32, yv(data[i])),
                color,
            );
        }
    }
}

fn render_scatter(
    img: &mut RgbaImage,
    series: &[Value],
    l: f64,
    pw: f64,
    xmin: f64,
    xmax: f64,
    yp: impl Fn(f64) -> f32,
) {
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        for (x, y) in series_points(s) {
            let px = l + (x - xmin) / (xmax - xmin) * pw;
            draw_filled_circle_mut(img, (px as i32, yp(y) as i32), 4, color);
        }
    }
}

fn render_bubble(
    img: &mut RgbaImage,
    series: &[Value],
    l: f64,
    pw: f64,
    xmin: f64,
    xmax: f64,
    yp: impl Fn(f64) -> f32,
) {
    let maxs = series
        .iter()
        .flat_map(series_points3)
        .map(|(_, _, s)| s)
        .fold(1.0f64, f64::max);
    for (si, s) in series.iter().enumerate() {
        let mut color = series_color(s, si);
        color.0[3] = 150;
        for (x, y, sz) in series_points3(s) {
            let px = l + (x - xmin) / (xmax - xmin) * pw;
            let r = (sz / maxs).sqrt() * 24.0;
            draw_filled_circle_mut(img, (px as i32, yp(y) as i32), r.max(2.0) as i32, color);
        }
    }
}

/// Radar/spider chart: each category is a spoke; each series is a polygon of
/// its values around the spokes.
#[allow(clippy::too_many_arguments)]
fn render_radar(
    img: &mut RgbaImage,
    fnt: &FontRef,
    series: &[Value],
    cats: &[String],
    l: i32,
    t: i32,
    r: i32,
    b: i32,
    black: Rgba<u8>,
    grid: Rgba<u8>,
) {
    let nax = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0).max(cats.len());
    if nax < 3 {
        return;
    }
    let cx = (l + r) / 2;
    let cy = (t + b) / 2;
    let radius = ((r - l).min(b - t) / 2 - 20).max(20) as f64;
    let maxv = series.iter().flat_map(series_nums).fold(1.0f64, f64::max);
    let ang = |i: usize| -std::f64::consts::FRAC_PI_2 + i as f64 / nax as f64 * std::f64::consts::TAU;
    // rings + spokes
    for ring in 1..=4 {
        let rr = radius * ring as f64 / 4.0;
        let pts: Vec<Point<i32>> = (0..nax)
            .map(|i| Point::new(cx + (rr * ang(i).cos()) as i32, cy + (rr * ang(i).sin()) as i32))
            .collect();
        for i in 0..nax {
            let a = pts[i];
            let bb = pts[(i + 1) % nax];
            draw_line_segment_mut(img, (a.x as f32, a.y as f32), (bb.x as f32, bb.y as f32), grid);
        }
    }
    for i in 0..nax {
        let x = cx + (radius * ang(i).cos()) as i32;
        let y = cy + (radius * ang(i).sin()) as i32;
        draw_line_segment_mut(img, (cx as f32, cy as f32), (x as f32, y as f32), grid);
        if let Some(c) = cats.get(i) {
            draw_text_mut(img, black, x - 10, y - 6, PxScale::from(11.0), fnt, c);
        }
    }
    // series polygons (outline via line segments)
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        let data = series_nums(s);
        let pt = |i: usize, v: f64| {
            let rr = v / maxv * radius;
            (cx as f32 + (rr * ang(i).cos()) as f32, cy as f32 + (rr * ang(i).sin()) as f32)
        };
        for i in 0..data.len() {
            let a = pt(i, data[i]);
            let bb = pt((i + 1) % data.len(), data[(i + 1) % data.len()]);
            draw_line_segment_mut(img, a, bb, color);
        }
    }
}

/// Raster sankey: distinct sources left, targets right, straight quad bands
/// proportional to value. (SVG output uses smooth bezier ribbons.)
fn render_sankey(img: &mut RgbaImage, opts: &Value, w: f64, h: f64) {
    let links = opts.get("links").and_then(Value::as_array).cloned().unwrap_or_default();
    if links.is_empty() {
        return;
    }
    let val = |lk: &Value| lk.get("value").and_then(Value::as_f64).unwrap_or(0.0);
    let src = |lk: &Value| lk.get("source").and_then(Value::as_u64).unwrap_or(0) as usize;
    let tgt = |lk: &Value| lk.get("target").and_then(Value::as_u64).unwrap_or(0) as usize;
    let total: f64 = links.iter().map(&val).sum();
    if total <= 0.0 {
        return;
    }
    let mut sources: Vec<usize> = links.iter().map(&src).collect();
    sources.sort_unstable();
    sources.dedup();
    let mut targets: Vec<usize> = links.iter().map(&tgt).collect();
    targets.sort_unstable();
    targets.dedup();
    let (lx, rx) = (80.0, w - 100.0);
    let (top, bot) = (50.0, h - 30.0);
    let avail = bot - top;
    let nodew = 16.0;
    let band = |ids: &[usize], is_src: bool| {
        let mut pos = std::collections::HashMap::new();
        let mut y = top;
        for &id in ids {
            let sum: f64 = links.iter().filter(|lk| if is_src { src(lk) == id } else { tgt(lk) == id }).map(&val).sum();
            let ht = sum / total * avail;
            pos.insert(id, (y, ht));
            y += ht + 8.0;
        }
        pos
    };
    let spos = band(&sources, true);
    let tpos = band(&targets, false);
    for (&_id, &(y, ht)) in spos.iter() {
        draw_filled_rect_mut(img, Rect::at(lx as i32, y as i32).of_size(nodew as u32, ht.max(1.0) as u32), parse_color(Some(&Value::String("#4472c4".into()))));
    }
    for (&_id, &(y, ht)) in tpos.iter() {
        draw_filled_rect_mut(img, Rect::at(rx as i32, y as i32).of_size(nodew as u32, ht.max(1.0) as u32), parse_color(Some(&Value::String("#ed7d31".into()))));
    }
    let mut soff: std::collections::HashMap<usize, f64> = std::collections::HashMap::new();
    let mut toff: std::collections::HashMap<usize, f64> = std::collections::HashMap::new();
    for (i, lk) in links.iter().enumerate() {
        let bh = val(lk) / total * avail;
        let (sy0, _) = *spos.get(&src(lk)).unwrap_or(&(top, 0.0));
        let (ty0, _) = *tpos.get(&tgt(lk)).unwrap_or(&(top, 0.0));
        let so = soff.entry(src(lk)).or_insert(0.0);
        let sy = sy0 + *so;
        *so += bh;
        let to = toff.entry(tgt(lk)).or_insert(0.0);
        let ty = ty0 + *to;
        *to += bh;
        let x0 = (lx + nodew) as i32;
        let x1 = rx as i32;
        let mut c = palette(i);
        c.0[3] = 120;
        let poly = vec![
            Point::new(x0, sy as i32),
            Point::new(x1, ty as i32),
            Point::new(x1, (ty + bh) as i32),
            Point::new(x0, (sy + bh) as i32),
        ];
        let mut p = poly;
        p.dedup();
        if p.len() >= 3 {
            draw_polygon_mut(img, &p, c);
        }
    }
}

fn render_pie(img: &mut RgbaImage, series: &[Value], l: i32, t: i32, r: i32, b: i32, donut: bool) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let total: f64 = data.iter().sum();
    if total <= 0.0 {
        return;
    }
    let cx = (l + r) / 2;
    let cy = (t + b) / 2;
    let radius = ((r - l).min(b - t) / 2 - 10).max(10) as f64;
    let mut angle = -std::f64::consts::FRAC_PI_2; // start at top
    for (i, v) in data.iter().enumerate() {
        let sweep = v / total * std::f64::consts::TAU;
        let steps = (sweep / 0.1).ceil().max(2.0) as usize;
        let mut poly: Vec<Point<i32>> = Vec::with_capacity(steps + 2);
        poly.push(Point::new(cx, cy));
        for k in 0..=steps {
            let a = angle + sweep * (k as f64 / steps as f64);
            poly.push(Point::new(
                cx + (radius * a.cos()) as i32,
                cy + (radius * a.sin()) as i32,
            ));
        }
        poly.dedup();
        if poly.len() >= 3 {
            draw_polygon_mut(img, &poly, palette(i));
        }
        angle += sweep;
    }
    if donut {
        draw_filled_circle_mut(img, (cx, cy), (radius * 0.55) as i32, Rgba([255, 255, 255, 255]));
    }
}
