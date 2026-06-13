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

fn op_chart_render(opts: Value) -> Result<Value> {
    let kind = opts.get("type").and_then(Value::as_str).unwrap_or("bar").to_string();
    let w = opts.get("width").and_then(Value::as_u64).unwrap_or(800).max(120) as u32;
    let h = opts.get("height").and_then(Value::as_u64).unwrap_or(600).max(120) as u32;
    let series = opts
        .get("series")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing series (expected array)"))?;
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

    if kind == "pie" {
        render_pie(&mut img, series, l, t, r, b);
        let handle = insert_image(DynamicImage::ImageRgba8(img));
        return Ok(json!({"handle": handle, "width": w, "height": h, "type": kind}));
    }

    // Cartesian: axes + y scale.
    draw_line_segment_mut(&mut img, (l as f32, b as f32), (r as f32, b as f32), black); // x axis
    draw_line_segment_mut(&mut img, (l as f32, t as f32), (l as f32, b as f32), black); // y axis

    let scatter = kind == "scatter";
    let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut xmin, mut xmax) = (f64::INFINITY, f64::NEG_INFINITY);
    if scatter {
        for s in series {
            for (x, y) in series_points(s) {
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
        _ => render_bars(&mut img, &fnt, series, &cats, l, pw, b, &yp, black),
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
) {
    let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0).max(cats.len());
    if ncat == 0 {
        return;
    }
    let nser = series.len().max(1);
    let slot = pw / ncat as f64;
    let barw = (slot * 0.8 / nser as f64).max(1.0);
    let base = yp(0.0);
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
    for (ci, cat) in cats.iter().enumerate() {
        let x = l as f64 + ci as f64 * slot + slot * 0.25;
        draw_text_mut(img, black, x as i32, b + 6, PxScale::from(12.0), fnt, cat);
    }
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

fn render_pie(img: &mut RgbaImage, series: &[Value], l: i32, t: i32, r: i32, b: i32) {
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
}
