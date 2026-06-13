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

thread_local! {
    /// Per-call custom color cycle (set from the `palette` opt); empty = use
    /// the built-in PALETTE.
    static PALETTE_OVERRIDE: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Install (or clear) the custom palette for this render call. Always called
/// once at the top of a render so state never leaks between calls.
fn set_palette(opts: &Value) {
    let v: Vec<String> = opts
        .get("palette")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();
    PALETTE_OVERRIDE.with(|p| *p.borrow_mut() = v);
}

fn palette(i: usize) -> Rgba<u8> {
    PALETTE_OVERRIDE.with(|p| {
        let p = p.borrow();
        let hex = if p.is_empty() {
            PALETTE[i % PALETTE.len()].to_string()
        } else {
            p[i % p.len()].clone()
        };
        parse_color(Some(&Value::String(hex)))
    })
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

/// OHLC tuples of a series (`data:[[open,high,low,close],...]`).
fn series_ohlc(s: &Value) -> Vec<[f64; 4]> {
    s.get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some([
                        p.first()?.as_f64()?,
                        p.get(1)?.as_f64()?,
                        p.get(2)?.as_f64()?,
                        p.get(3)?.as_f64()?,
                    ])
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Five-number summary (min, q1, median, q3, max) of a sorted-able slice.
fn five_number(data: &[f64]) -> Option<[f64; 5]> {
    if data.is_empty() {
        return None;
    }
    let mut v = data.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q = |p: f64| {
        let idx = (p * (v.len() - 1) as f64).round() as usize;
        v[idx.min(v.len() - 1)]
    };
    Some([v[0], q(0.25), q(0.5), q(0.75), v[v.len() - 1]])
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
    let labels = opts.get("labels").and_then(Value::as_bool).unwrap_or(false);
    let smooth = opts.get("smooth").and_then(Value::as_bool).unwrap_or(false);
    set_palette(&opts);
    let bg = match opts.get("background") {
        Some(c) => parse_color(Some(c)),
        None => Rgba([255, 255, 255, 255]),
    };

    let mut img = RgbaImage::from_pixel(w, h, bg);
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

    // Kinds that don't need a `series` array (or use a different shape).
    let needs_series = !matches!(kind.as_str(), "sankey" | "gauge" | "heatmap" | "sunburst" | "calendar");

    // Non-cartesian renderers draw into `img` then we fall through to the
    // shared legend/finish path. `false` => handle below in the cartesian arm.
    let mut special = true;
    match kind.as_str() {
        "sankey" => render_sankey(&mut img, &opts, w as f64, h as f64),
        "radar" => {
            require_series(&opts)?;
            render_radar(&mut img, &fnt, series, &cats, l, t, r, b, black, grid)
        }
        "pie" | "donut" | "doughnut" => {
            require_series(&opts)?;
            render_pie(&mut img, &fnt, series, &cats, l, t, r, b, kind != "pie", labels, black)
        }
        "funnel" => {
            require_series(&opts)?;
            render_funnel(&mut img, &fnt, series, &cats, l, t, r, b, labels, black)
        }
        "gauge" => render_gauge(&mut img, &fnt, &opts, l, t, r, b, black),
        "heatmap" => render_heatmap(&mut img, &fnt, &opts, &cats, l, t, r, b, black),
        "treemap" => {
            require_series(&opts)?;
            render_treemap(&mut img, &fnt, series, &cats, l, t, r, b, black)
        }
        "polar" => {
            require_series(&opts)?;
            render_polar(&mut img, &fnt, series, &cats, l, t, r, b, black, grid)
        }
        "bullet" => {
            require_series(&opts)?;
            render_bullet(&mut img, &fnt, series, l, t, r, b, black, grid)
        }
        "pareto" => {
            require_series(&opts)?;
            render_pareto(&mut img, &fnt, series, &cats, l, t, r, b, black, grid)
        }
        "gantt" => {
            require_series(&opts)?;
            render_gantt(&mut img, &fnt, series, l, t, r, b, black, grid)
        }
        "sunburst" => render_sunburst(&mut img, &opts, series, l, t, r, b),
        "waffle" => {
            require_series(&opts)?;
            render_waffle(&mut img, series, &cats, l, t, r, b)
        }
        "slope" => {
            require_series(&opts)?;
            render_slope(&mut img, &fnt, series, l, t, r, b, black)
        }
        "marimekko" | "mosaic" => {
            require_series(&opts)?;
            render_marimekko(&mut img, &fnt, series, &cats, l, t, r, b, black)
        }
        "radial_bar" => {
            require_series(&opts)?;
            render_radial_bar(&mut img, &fnt, series, &cats, l, t, r, b, black)
        }
        "calendar" => render_calendar(&mut img, &opts, series, l, t, r, b),
        "parallel" => {
            require_series(&opts)?;
            render_parallel(&mut img, &fnt, series, &cats, l, t, r, b, black, grid)
        }
        "hexbin" => {
            require_series(&opts)?;
            render_hexbin(&mut img, &opts, series, l, t, r, b)
        }
        _ => special = false,
    }

    if !special {
        if needs_series {
            require_series(&opts)?;
        }
        // Cartesian: axes + y scale.
        draw_line_segment_mut(&mut img, (l as f32, b as f32), (r as f32, b as f32), black); // x axis
        draw_line_segment_mut(&mut img, (l as f32, t as f32), (l as f32, b as f32), black); // y axis

        let scatter = kind == "scatter" || kind == "bubble";
        let ohlc_like = kind == "ohlc" || kind == "candlestick";
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
        } else if ohlc_like {
            for s in series {
                for o in series_ohlc(s) {
                    ymin = ymin.min(o[2]);
                    ymax = ymax.max(o[1]);
                }
            }
        } else if kind == "waterfall" {
            let mut cum = 0.0;
            ymin = ymin.min(0.0);
            ymax = ymax.max(0.0);
            for v in series.first().map(series_nums).unwrap_or_default() {
                cum += v;
                ymin = ymin.min(cum);
                ymax = ymax.max(cum);
            }
        } else if kind == "stacked_area" {
            let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0);
            ymin = ymin.min(0.0);
            for ci in 0..ncat {
                let col: f64 = series.iter().map(|s| series_nums(s).get(ci).copied().unwrap_or(0.0)).sum();
                ymax = ymax.max(col);
            }
        } else if matches!(kind.as_str(), "range" | "range_bar" | "range_column") {
            for s in series {
                for (lo, hi) in series_pairs(s) {
                    ymin = ymin.min(lo);
                    ymax = ymax.max(hi);
                }
            }
        } else if kind == "percent_stacked" {
            ymin = 0.0;
            ymax = 100.0;
        } else if kind == "streamgraph" {
            let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0);
            let mut maxtot = 0.0f64;
            for ci in 0..ncat {
                let col: f64 = series.iter().map(|s| series_nums(s).get(ci).copied().unwrap_or(0.0)).sum();
                maxtot = maxtot.max(col);
            }
            ymax = maxtot / 2.0;
            ymin = -maxtot / 2.0;
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
        // Optional logarithmic Y (only when the whole range is positive).
        let logy = opts.get("log_y").and_then(Value::as_bool) == Some(true) && ymin > 0.0;
        let lmin = ymin.max(1e-9).log10();
        let lspan = (ymax.max(1e-9).log10() - lmin).max(1e-9);
        let yp = |v: f64| {
            let frac = if logy {
                (v.max(1e-9).log10() - lmin) / lspan
            } else {
                (v - ymin) / (ymax - ymin)
            };
            (b as f64 - frac * ph) as f32
        };

        // Y gridlines + tick labels (5 ticks).
        for i in 0..=5 {
            let v = if logy {
                10f64.powf(lmin + lspan * i as f64 / 5.0)
            } else {
                ymin + (ymax - ymin) * (i as f64) / 5.0
            };
            let y = yp(v);
            draw_line_segment_mut(&mut img, (l as f32, y), (r as f32, y), grid);
            draw_text_mut(&mut img, black, 4, y as i32 - 6, PxScale::from(12.0), &fnt, &fmt_num(v));
        }

        match kind.as_str() {
            "line" => render_line_area(&mut img, series, l, r, b, ymin, ymax, false, smooth),
            "area" => render_line_area(&mut img, series, l, r, b, ymin, ymax, true, smooth),
            "stacked_area" => render_stacked_area(&mut img, series, l, r, b, ymin, ymax),
            "streamgraph" => render_streamgraph(&mut img, series, l, r, b, ymin, ymax),
            "range" | "range_bar" | "range_column" => render_range(&mut img, series, l, pw, &yp),
            "percent_stacked" => render_percent_stacked(&mut img, series, l, pw, b, &yp),
            "step" => render_step(&mut img, series, l, r, b, ymin, ymax),
            "scatter" => render_scatter(&mut img, series, l as f64, pw, xmin, xmax, yp),
            "bubble" => render_bubble(&mut img, series, l as f64, pw, xmin, xmax, yp),
            "histogram" => render_histogram(&mut img, &fnt, series, &opts, l, pw, t, b, black),
            "stacked" | "stacked_bar" => {
                render_bars(&mut img, &fnt, series, &cats, l, pw, b, &yp, black, true, labels)
            }
            "combo" => render_combo(&mut img, &fnt, series, &cats, l, r, pw, b, &yp, ymin, ymax, black, labels),
            "waterfall" => render_waterfall(&mut img, &fnt, series, &cats, l, pw, b, &yp, black, labels),
            "ohlc" => render_ohlc(&mut img, series, l, pw, b, &yp, false),
            "candlestick" => render_ohlc(&mut img, series, l, pw, b, &yp, true),
            "boxplot" => render_boxplot(&mut img, series, l, pw, b, &yp),
            "lollipop" => render_lollipop(&mut img, series, l, pw, b, &yp, true),
            "dot" => render_lollipop(&mut img, series, l, pw, b, &yp, false),
            _ => render_bars(&mut img, &fnt, series, &cats, l, pw, b, &yp, black, false, labels),
        }
        // markers on line-family points
        if opts.get("markers").and_then(Value::as_bool) == Some(true)
            && matches!(kind.as_str(), "line" | "area" | "step" | "stacked_area")
        {
            for (si, s) in series.iter().enumerate() {
                let color = series_color(s, si);
                let data = series_nums(s);
                let n = data.len();
                for (i, v) in data.iter().enumerate() {
                    let x = l as f64 + if n > 1 { i as f64 / (n - 1) as f64 * pw } else { pw / 2.0 };
                    draw_filled_circle_mut(&mut img, (x as i32, yp(*v) as i32), 3, color);
                }
            }
        }
        // horizontal reference lines: `reference_lines:[{y, color?}]`
        if let Some(refs) = opts.get("reference_lines").and_then(Value::as_array) {
            for rl in refs {
                if let Some(yv) = rl.get("y").and_then(Value::as_f64) {
                    let color = parse_color(rl.get("color").or(Some(&Value::String("#cc3333".into()))));
                    let y = yp(yv);
                    let mut x = l as f32;
                    while x < r as f32 {
                        draw_line_segment_mut(&mut img, (x, y), ((x + 6.0).min(r as f32), y), color);
                        x += 12.0;
                    }
                }
            }
        }
        // optional least-squares trendline over scatter points
        if scatter && opts.get("trendline").and_then(Value::as_bool) == Some(true) {
            for (si, s) in series.iter().enumerate() {
                let pts = series_points(s);
                if let Some((m, c)) = linfit(&pts) {
                    let color = series_color(s, si);
                    let x0 = xmin;
                    let x1 = xmax;
                    let px = |x: f64| l as f64 + (x - xmin) / (xmax - xmin) * pw;
                    draw_line_segment_mut(&mut img, (px(x0) as f32, yp(m * x0 + c)), (px(x1) as f32, yp(m * x1 + c)), color);
                }
            }
        }
        // error bars: per-series `errors` array parallel to `data`
        if matches!(kind.as_str(), "bar" | "column" | "line" | "area" | "scatter") {
            let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0).max(1);
            for s in series {
                let Some(errs) = s.get("errors").and_then(Value::as_array) else { continue };
                let data = series_nums(s);
                for (ci, e) in errs.iter().filter_map(Value::as_f64).enumerate() {
                    let Some(&v) = data.get(ci) else { continue };
                    let x = if scatter {
                        let pts = series_points(s);
                        match pts.get(ci) {
                            Some((px, _)) => l as f64 + (px - xmin) / (xmax - xmin) * pw,
                            None => continue,
                        }
                    } else {
                        l as f64 + (ci as f64 + 0.5) / ncat as f64 * pw
                    };
                    let (yhi, ylo) = (yp(v + e), yp(v - e));
                    draw_line_segment_mut(&mut img, (x as f32, yhi), (x as f32, ylo), black);
                    draw_line_segment_mut(&mut img, (x as f32 - 3.0, yhi), (x as f32 + 3.0, yhi), black);
                    draw_line_segment_mut(&mut img, (x as f32 - 3.0, ylo), (x as f32 + 3.0, ylo), black);
                }
            }
        }
        // annotations: [{x (data index), y (value), text, color?}]
        if let Some(anns) = opts.get("annotations").and_then(Value::as_array) {
            let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(1).max(1);
            for a in anns {
                let (Some(xi), Some(yv)) = (a.get("x").and_then(Value::as_f64), a.get("y").and_then(Value::as_f64)) else { continue };
                let x = l as f64 + if ncat > 1 { xi / (ncat - 1) as f64 * pw } else { pw / 2.0 };
                let color = parse_color(a.get("color").or(Some(&Value::String("#c0392b".into()))));
                draw_filled_circle_mut(&mut img, (x as i32, yp(yv) as i32), 4, color);
                if let Some(txt) = a.get("text").and_then(Value::as_str) {
                    draw_text_mut(&mut img, black, x as i32 + 6, yp(yv) as i32 - 6, PxScale::from(11.0), &fnt, txt);
                }
            }
        }
        // category labels under the x axis for the bar-family
        if matches!(kind.as_str(), "waterfall" | "combo" | "ohlc" | "candlestick" | "boxplot") {
            let n = cats.len();
            let slot = if n > 0 { pw / n as f64 } else { pw };
            for (ci, cat) in cats.iter().enumerate() {
                let x = l as f64 + ci as f64 * slot + slot * 0.25;
                draw_text_mut(&mut img, black, x as i32, b + 6, PxScale::from(12.0), &fnt, cat);
            }
        }
        draw_axis_titles(&mut img, &fnt, &opts, l, r, t, b, black);
    }

    // Shared legend (series names, or pie/funnel categories).
    if opts.get("legend").and_then(Value::as_bool) != Some(false) {
        let entries = legend_entries(&kind, series, &cats);
        draw_legend(&mut img, &fnt, &entries, w as i32, t, black);
    }

    let handle = insert_image(DynamicImage::ImageRgba8(img));
    Ok(json!({"handle": handle, "width": w, "height": h, "type": kind}))
}

/// Error unless an `series` array is present.
fn require_series(opts: &Value) -> Result<()> {
    if opts.get("series").and_then(Value::as_array).is_none() {
        return Err(anyhow!("missing series (expected array)"));
    }
    Ok(())
}

/// (label, color) legend entries: category slices for pie/funnel, otherwise
/// the named series.
fn legend_entries(kind: &str, series: &[Value], cats: &[String]) -> Vec<(String, Rgba<u8>)> {
    if matches!(kind, "pie" | "donut" | "doughnut" | "funnel") {
        cats.iter().enumerate().map(|(i, c)| (c.clone(), palette(i))).collect()
    } else {
        series
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                s.get("name").and_then(Value::as_str).map(|n| (n.to_string(), series_color(s, i)))
            })
            .collect()
    }
}

/// Top-right legend with color swatches.
fn draw_legend(img: &mut RgbaImage, fnt: &FontRef, entries: &[(String, Rgba<u8>)], w: i32, t: i32, black: Rgba<u8>) {
    if entries.is_empty() {
        return;
    }
    let box_w = 1 + entries.iter().map(|(n, _)| n.len()).max().unwrap_or(4) as i32 * 7 + 22;
    let x0 = (w - box_w - 10).max(0);
    let mut y = t + 4;
    for (name, color) in entries {
        draw_filled_rect_mut(img, Rect::at(x0, y).of_size(12, 12), *color);
        draw_text_mut(img, black, x0 + 16, y - 1, PxScale::from(12.0), fnt, name);
        y += 18;
    }
}

/// X/Y axis titles from `x_label` / `y_label` opts.
#[allow(clippy::too_many_arguments)]
fn draw_axis_titles(img: &mut RgbaImage, fnt: &FontRef, opts: &Value, l: i32, r: i32, _t: i32, b: i32, black: Rgba<u8>) {
    if let Some(x) = opts.get("x_label").and_then(Value::as_str) {
        let cx = (l + r) / 2 - x.len() as i32 * 4;
        draw_text_mut(img, black, cx, b + 22, PxScale::from(14.0), fnt, x);
    }
    if let Some(y) = opts.get("y_label").and_then(Value::as_str) {
        // raster text can't rotate cheaply; place it at the top of the y axis
        draw_text_mut(img, black, l - 52, 26, PxScale::from(14.0), fnt, y);
    }
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
    labels: bool,
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
                if labels {
                    draw_text_mut(img, black, x as i32, top as i32 - 14, PxScale::from(11.0), fnt, &fmt_num(v));
                }
            }
        }
    }
    for (ci, cat) in cats.iter().enumerate() {
        let x = l as f64 + ci as f64 * slot + slot * 0.25;
        draw_text_mut(img, black, x as i32, b + 6, PxScale::from(12.0), fnt, cat);
    }
}

/// Compact number formatting for data labels.
fn fmt_num(v: f64) -> String {
    if v.fract().abs() < 1e-9 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
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
    smooth: bool,
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
        // The drawn vertices — Catmull-Rom-interpolated when smoothing.
        let verts: Vec<(f32, f32)> = if smooth && n >= 3 {
            catmull_rom(&(0..n).map(|i| (xat(i) as f32, yv(data[i]))).collect::<Vec<_>>(), 16)
        } else {
            (0..n).map(|i| (xat(i) as f32, yv(data[i]))).collect()
        };
        if fill {
            let mut poly: Vec<Point<i32>> = Vec::with_capacity(verts.len() + 2);
            poly.push(Point::new(xat(0) as i32, b));
            for &(x, y) in &verts {
                poly.push(Point::new(x as i32, y as i32));
            }
            poly.push(Point::new(xat(n - 1) as i32, b));
            poly.dedup();
            if poly.len() >= 3 && poly.first() != poly.last() {
                let mut fillc = color;
                fillc.0[3] = 120;
                draw_polygon_mut(img, &poly, fillc);
            }
        }
        for w in verts.windows(2) {
            draw_line_segment_mut(img, w[0], w[1], color);
        }
    }
}

/// Sample a Catmull-Rom spline through `pts`, `steps` segments between each
/// pair, returning the densified polyline.
fn catmull_rom(pts: &[(f32, f32)], steps: usize) -> Vec<(f32, f32)> {
    let n = pts.len();
    if n < 3 {
        return pts.to_vec();
    }
    let mut out = Vec::with_capacity(n * steps);
    let at = |i: isize| pts[i.clamp(0, n as isize - 1) as usize];
    for i in 0..n - 1 {
        let p0 = at(i as isize - 1);
        let p1 = pts[i];
        let p2 = pts[i + 1];
        let p3 = at(i as isize + 2);
        for s in 0..steps {
            let t = s as f32 / steps as f32;
            let t2 = t * t;
            let t3 = t2 * t;
            let f = |a: f32, b: f32, c: f32, d: f32| {
                0.5 * ((2.0 * b) + (-a + c) * t + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2 + (-a + 3.0 * b - 3.0 * c + d) * t3)
            };
            out.push((f(p0.0, p1.0, p2.0, p3.0), f(p0.1, p1.1, p2.1, p3.1)));
        }
    }
    out.push(pts[n - 1]);
    out
}

/// `[low, high]` pairs of a series (`data:[[lo,hi],...]`).
fn series_pairs(s: &Value) -> Vec<(f64, f64)> {
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

#[allow(clippy::too_many_arguments)]
fn render_pie(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, donut: bool, labels: bool, black: Rgba<u8>) {
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
        if labels {
            let mid = angle + sweep / 2.0;
            let lx = cx + (radius * 0.65 * mid.cos()) as i32;
            let ly = cy + (radius * 0.65 * mid.sin()) as i32;
            let pct = v / total * 100.0;
            let text = cats.get(i).map(|c| format!("{c} {pct:.0}%")).unwrap_or_else(|| format!("{pct:.0}%"));
            draw_text_mut(img, black, lx - 14, ly - 6, PxScale::from(11.0), fnt, &text);
        }
        angle += sweep;
    }
    if donut {
        draw_filled_circle_mut(img, (cx, cy), (radius * 0.55) as i32, Rgba([255, 255, 255, 255]));
    }
}

/// Stepped line: hold each value then jump to the next.
#[allow(clippy::too_many_arguments)]
fn render_step(img: &mut RgbaImage, series: &[Value], l: i32, r: i32, b: i32, ymin: f64, ymax: f64, ) {
    let pw = (r - l).max(1) as f64;
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        let data = series_nums(s);
        let n = data.len();
        if n == 0 {
            continue;
        }
        let xat = |i: usize| l as f64 + if n > 1 { i as f64 / (n - 1) as f64 * pw } else { pw / 2.0 };
        let yv = |v: f64| (b as f64 - (v - ymin) / (ymax - ymin) * (b as f64 - 44.0).max(1.0)) as f32;
        for i in 1..n {
            let (x0, x1) = (xat(i - 1) as f32, xat(i) as f32);
            let (y0, y1) = (yv(data[i - 1]), yv(data[i]));
            draw_line_segment_mut(img, (x0, y0), (x1, y0), color); // horizontal hold
            draw_line_segment_mut(img, (x1, y0), (x1, y1), color); // vertical step
        }
    }
}

/// Combination chart: each series renders as a bar (default) or a line when
/// it carries `kind:"line"`, sharing one y scale.
#[allow(clippy::too_many_arguments)]
fn render_combo(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, r: i32, pw: f64, b: i32, yp: &dyn Fn(f64) -> f32, ymin: f64, ymax: f64, black: Rgba<u8>, labels: bool) {
    let bar_series: Vec<&Value> = series.iter().filter(|s| s.get("kind").and_then(Value::as_str) != Some("line")).collect();
    let line_series: Vec<&Value> = series.iter().filter(|s| s.get("kind").and_then(Value::as_str) == Some("line")).collect();
    let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0).max(cats.len());
    if ncat == 0 {
        return;
    }
    let slot = pw / ncat as f64;
    let base = yp(0.0);
    let nbar = bar_series.len().max(1);
    let barw = (slot * 0.7 / nbar as f64).max(1.0);
    for (bi, s) in bar_series.iter().enumerate() {
        let color = series_color(s, series.iter().position(|x| std::ptr::eq(x, *s)).unwrap_or(bi));
        for (ci, v) in series_nums(s).into_iter().enumerate() {
            let x = l as f64 + ci as f64 * slot + slot * 0.15 + bi as f64 * barw;
            let top = yp(v);
            let (y0, y1) = if top < base { (top, base) } else { (base, top) };
            draw_filled_rect_mut(img, Rect::at(x as i32, y0 as i32).of_size(barw as u32, (y1 - y0).max(1.0) as u32), color);
            if labels {
                draw_text_mut(img, black, x as i32, top as i32 - 14, PxScale::from(11.0), fnt, &fmt_num(v));
            }
        }
    }
    for s in &line_series {
        let color = series_color(s, series.iter().position(|x| std::ptr::eq(x, *s)).unwrap_or(0));
        let data = series_nums(s);
        let n = data.len();
        let xat = |i: usize| l as f64 + ci_center(i, slot);
        let yv = |v: f64| (b as f64 - (v - ymin) / (ymax - ymin) * (b as f64 - 44.0).max(1.0)) as f32;
        for i in 1..n {
            draw_line_segment_mut(img, (xat(i - 1) as f32, yv(data[i - 1])), (xat(i) as f32, yv(data[i])), color);
        }
        for (i, v) in data.iter().enumerate() {
            draw_filled_circle_mut(img, (xat(i) as i32, yv(*v) as i32), 3, color);
        }
    }
    let _ = r;
}

/// Center x of category `i` within its slot.
fn ci_center(i: usize, slot: f64) -> f64 {
    i as f64 * slot + slot * 0.5
}

/// Waterfall: running cumulative bars from the first series' deltas; rising
/// steps in green, falling in red, with connector lines.
#[allow(clippy::too_many_arguments)]
fn render_waterfall(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, pw: f64, _b: i32, yp: &dyn Fn(f64) -> f32, black: Rgba<u8>, labels: bool) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let slot = pw / n as f64;
    let up = Rgba([0x55, 0xaa, 0x55, 255]);
    let down = Rgba([0xcc, 0x55, 0x55, 255]);
    let barw = (slot * 0.7).max(1.0);
    let mut cum = 0.0;
    for (i, &v) in data.iter().enumerate() {
        let prev = cum;
        cum += v;
        let (y0, y1) = (yp(prev.max(cum)), yp(prev.min(cum)));
        let x = l as f64 + i as f64 * slot + slot * 0.15;
        let color = if v >= 0.0 { up } else { down };
        draw_filled_rect_mut(img, Rect::at(x as i32, y0 as i32).of_size(barw as u32, (y1 - y0).max(1.0) as u32), color);
        if i + 1 < n {
            let yc = yp(cum);
            draw_line_segment_mut(img, ((x + barw) as f32, yc), ((x + slot) as f32, yc), Rgba([150, 150, 150, 255]));
        }
        if labels {
            draw_text_mut(img, black, x as i32, y0 as i32 - 14, PxScale::from(11.0), fnt, &fmt_num(v));
        }
        let _ = cats;
    }
}

/// OHLC bars or candlesticks. `data:[[open,high,low,close],...]` for the
/// first series.
fn render_ohlc(img: &mut RgbaImage, series: &[Value], l: i32, pw: f64, b: i32, yp: &dyn Fn(f64) -> f32, candle: bool) {
    use imageproc::drawing::draw_hollow_rect_mut;
    let data = series.first().map(series_ohlc).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let slot = pw / n as f64;
    let up = Rgba([0x33, 0x99, 0x55, 255]);
    let down = Rgba([0xcc, 0x44, 0x44, 255]);
    let bw = (slot * 0.5).max(2.0);
    for (i, o) in data.iter().enumerate() {
        let [open, high, low, close] = *o;
        let cx = l as f64 + i as f64 * slot + slot * 0.5;
        let color = if close >= open { up } else { down };
        // wick
        draw_line_segment_mut(img, (cx as f32, yp(high)), (cx as f32, yp(low)), color);
        if candle {
            let (top, bot) = (open.max(close), open.min(close));
            let y = yp(top);
            let height = (yp(bot) - y).max(1.0);
            let rect = Rect::at((cx - bw / 2.0) as i32, y as i32).of_size(bw as u32, height as u32);
            if close >= open {
                draw_hollow_rect_mut(img, rect, color);
            } else {
                draw_filled_rect_mut(img, rect, color);
            }
        } else {
            // open tick (left), close tick (right)
            draw_line_segment_mut(img, ((cx - bw / 2.0) as f32, yp(open)), (cx as f32, yp(open)), color);
            draw_line_segment_mut(img, (cx as f32, yp(close)), ((cx + bw / 2.0) as f32, yp(close)), color);
        }
        let _ = b;
    }
}

/// Box-and-whisker per series: min / q1 / median / q3 / max from raw data.
fn render_boxplot(img: &mut RgbaImage, series: &[Value], l: i32, pw: f64, b: i32, yp: &dyn Fn(f64) -> f32) {
    let n = series.len().max(1);
    let slot = pw / n as f64;
    let bw = (slot * 0.4).max(3.0);
    for (si, s) in series.iter().enumerate() {
        let Some([mn, q1, med, q3, mx]) = five_number(&series_nums(s)) else { continue };
        let color = series_color(s, si);
        let cx = l as f64 + si as f64 * slot + slot * 0.5;
        let (x0, x1) = ((cx - bw / 2.0) as f32, (cx + bw / 2.0) as f32);
        // whiskers
        draw_line_segment_mut(img, (cx as f32, yp(mn)), (cx as f32, yp(q1)), color);
        draw_line_segment_mut(img, (cx as f32, yp(q3)), (cx as f32, yp(mx)), color);
        draw_line_segment_mut(img, (x0, yp(mn)), (x1, yp(mn)), color);
        draw_line_segment_mut(img, (x0, yp(mx)), (x1, yp(mx)), color);
        // box (q1..q3) + median line
        let y = yp(q3);
        let height = (yp(q1) - y).max(1.0);
        use imageproc::drawing::draw_hollow_rect_mut;
        draw_hollow_rect_mut(img, Rect::at(x0 as i32, y as i32).of_size(bw as u32, height as u32), color);
        draw_line_segment_mut(img, (x0, yp(med)), (x1, yp(med)), color);
        let _ = b;
    }
}

/// Funnel: centered descending bands from the first series' values.
#[allow(clippy::too_many_arguments)]
fn render_funnel(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, labels: bool, black: Rgba<u8>) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let maxv = data.iter().cloned().fold(0.0f64, f64::max).max(f64::EPSILON);
    let cx = (l + r) / 2;
    let fullw = (r - l) as f64 * 0.8;
    let band_h = ((b - t) as f64 / n as f64) * 0.85;
    let gap = ((b - t) as f64 / n as f64) * 0.15;
    for (i, &v) in data.iter().enumerate() {
        let half = fullw * (v / maxv) / 2.0;
        let y = t as f64 + i as f64 * (band_h + gap);
        let poly = vec![
            Point::new((cx as f64 - half) as i32, y as i32),
            Point::new((cx as f64 + half) as i32, y as i32),
            Point::new((cx as f64 + half) as i32, (y + band_h) as i32),
            Point::new((cx as f64 - half) as i32, (y + band_h) as i32),
        ];
        let mut p = poly;
        p.dedup();
        if p.len() >= 3 {
            draw_polygon_mut(img, &p, palette(i));
        }
        if labels {
            let text = cats.get(i).map(|c| format!("{c}: {}", fmt_num(v))).unwrap_or_else(|| fmt_num(v));
            draw_text_mut(img, black, cx - text.len() as i32 * 3, (y + band_h / 2.0 - 6.0) as i32, PxScale::from(12.0), fnt, &text);
        }
    }
}

/// Semicircular gauge: `value` against `max` (default 100).
#[allow(clippy::too_many_arguments)]
fn render_gauge(img: &mut RgbaImage, fnt: &FontRef, opts: &Value, l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>) {
    let value = opts.get("value").and_then(Value::as_f64).unwrap_or(0.0);
    let max = opts.get("max").and_then(Value::as_f64).unwrap_or(100.0).max(f64::EPSILON);
    let frac = (value / max).clamp(0.0, 1.0);
    let cx = (l + r) / 2;
    let cy = (t + b) * 2 / 3;
    let radius = ((r - l).min((b - t) * 2) / 2 - 20).max(20) as f64;
    let thick = (radius * 0.28) as i32;
    // background arc (gray) then filled arc (palette) over the top semicircle
    let arc = |img: &mut RgbaImage, frac: f64, color: Rgba<u8>| {
        let start = std::f64::consts::PI; // 180°
        let steps = 120;
        for k in 0..=((steps as f64 * frac) as usize) {
            let a = start + (k as f64 / steps as f64) * std::f64::consts::PI;
            let x = cx + (radius * a.cos()) as i32;
            let y = cy + (radius * a.sin()) as i32;
            draw_filled_circle_mut(img, (x, y), thick / 2, color);
        }
    };
    arc(img, 1.0, Rgba([220, 220, 220, 255]));
    arc(img, frac, palette(0));
    let text = format!("{}/{}", fmt_num(value), fmt_num(max));
    draw_text_mut(img, black, cx - text.len() as i32 * 5, cy - 10, PxScale::from(22.0), fnt, &text);
}

/// Heatmap of a `matrix:[[..],..]` (rows) or series-of-data, colored
/// white→blue by normalized value.
#[allow(clippy::too_many_arguments)]
fn render_heatmap(img: &mut RgbaImage, fnt: &FontRef, opts: &Value, cats: &[String], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>) {
    // rows: explicit matrix, else each series' data is a row
    let rows: Vec<Vec<f64>> = if let Some(m) = opts.get("matrix").and_then(Value::as_array) {
        m.iter().map(|row| row.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default()).collect()
    } else {
        opts.get("series").and_then(Value::as_array).map(|s| s.iter().map(series_nums).collect()).unwrap_or_default()
    };
    let nr = rows.len();
    let nc = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if nr == 0 || nc == 0 {
        return;
    }
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for row in &rows {
        for &v in row {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    let span = if (hi - lo).abs() < f64::EPSILON { 1.0 } else { hi - lo };
    let cw = (r - l) as f64 / nc as f64;
    let ch = (b - t) as f64 / nr as f64;
    for (ri, row) in rows.iter().enumerate() {
        for (ci, &v) in row.iter().enumerate() {
            let frac = ((v - lo) / span).clamp(0.0, 1.0);
            // white (low) -> deep blue (high)
            let color = Rgba([
                (255.0 - frac * 187.0) as u8,
                (255.0 - frac * 141.0) as u8,
                255,
                255,
            ]);
            let x = l as f64 + ci as f64 * cw;
            let y = t as f64 + ri as f64 * ch;
            draw_filled_rect_mut(img, Rect::at(x as i32, y as i32).of_size(cw.max(1.0) as u32, ch.max(1.0) as u32), color);
        }
    }
    for (ci, cat) in cats.iter().enumerate().take(nc) {
        let x = l as f64 + ci as f64 * cw + cw * 0.2;
        draw_text_mut(img, black, x as i32, b + 6, PxScale::from(11.0), fnt, cat);
    }
}

/// Least-squares fit of `points` → `(slope, intercept)`, or None if
/// degenerate.
fn linfit(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = points.len() as f64;
    if n < 2.0 {
        return None;
    }
    let (sx, sy) = points.iter().fold((0.0, 0.0), |(ax, ay), &(x, y)| (ax + x, ay + y));
    let (sxx, sxy) = points.iter().fold((0.0, 0.0), |(axx, axy), &(x, y)| (axx + x * x, axy + x * y));
    let denom = n * sxx - sx * sx;
    if denom.abs() < f64::EPSILON {
        return None;
    }
    let m = (n * sxy - sx * sy) / denom;
    Some((m, (sy - m * sx) / n))
}

/// Stacked area: each series' band is filled on top of the cumulative sum.
fn render_stacked_area(img: &mut RgbaImage, series: &[Value], l: i32, r: i32, b: i32, ymin: f64, ymax: f64) {
    let pw = (r - l).max(1) as f64;
    let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let xat = |i: usize| l as f64 + if ncat > 1 { i as f64 / (ncat - 1) as f64 * pw } else { pw / 2.0 };
    let yv = |v: f64| (b as f64 - (v - ymin) / (ymax - ymin) * (b as f64 - 44.0).max(1.0)) as f32;
    let mut cum = vec![0.0f64; ncat];
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        let data = series_nums(s);
        let mut poly: Vec<Point<i32>> = Vec::new();
        // top edge (cum + value) left→right
        for ci in 0..ncat {
            let v = data.get(ci).copied().unwrap_or(0.0);
            poly.push(Point::new(xat(ci) as i32, yv(cum[ci] + v) as i32));
        }
        // bottom edge (cum) right→left
        for ci in (0..ncat).rev() {
            poly.push(Point::new(xat(ci) as i32, yv(cum[ci]) as i32));
        }
        poly.dedup();
        if poly.len() >= 3 && poly.first() != poly.last() {
            let mut fc = color;
            fc.0[3] = 150;
            draw_polygon_mut(img, &poly, fc);
        }
        for ci in 0..ncat {
            cum[ci] += data.get(ci).copied().unwrap_or(0.0);
        }
    }
}

/// Squarish treemap of the first series via area-correct recursive binary
/// split (sorted desc, halved by cumulative value, split along the long axis).
#[allow(clippy::too_many_arguments)]
fn render_treemap(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let mut items: Vec<(usize, f64)> = data.iter().enumerate().map(|(i, &v)| (i, v.max(0.0))).collect();
    items.retain(|&(_, v)| v > 0.0);
    if items.is_empty() {
        return;
    }
    items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let rect = (l as f64, t as f64, (r - l) as f64, (b - t) as f64);
    let mut placed: Vec<(usize, (f64, f64, f64, f64))> = Vec::new();
    treemap_layout(&items, rect, &mut placed);
    for (idx, (x, y, w, h)) in placed {
        draw_filled_rect_mut(img, Rect::at(x as i32 + 1, y as i32 + 1).of_size((w as u32).saturating_sub(2).max(1), (h as u32).saturating_sub(2).max(1)), palette(idx));
        if let Some(name) = cats.get(idx) {
            if w > 24.0 && h > 14.0 {
                draw_text_mut(img, black, x as i32 + 4, y as i32 + 4, PxScale::from(11.0), fnt, name);
            }
        }
    }
}

fn treemap_layout(items: &[(usize, f64)], rect: (f64, f64, f64, f64), out: &mut Vec<(usize, (f64, f64, f64, f64))>) {
    if items.len() == 1 {
        out.push((items[0].0, rect));
        return;
    }
    let total: f64 = items.iter().map(|&(_, v)| v).sum();
    let half = total / 2.0;
    let (mut acc, mut i) = (0.0, 0usize);
    while i < items.len() - 1 && acc + items[i].1 < half {
        acc += items[i].1;
        i += 1;
    }
    let (a, c) = items.split_at(i + 1);
    let suma: f64 = a.iter().map(|&(_, v)| v).sum();
    let frac = if total > 0.0 { suma / total } else { 0.5 };
    let (x, y, w, h) = rect;
    let (ra, rc) = if w >= h {
        ((x, y, w * frac, h), (x + w * frac, y, w * (1.0 - frac), h))
    } else {
        ((x, y, w, h * frac), (x, y + h * frac, w, h * (1.0 - frac)))
    };
    treemap_layout(a, ra, out);
    treemap_layout(c, rc, out);
}

/// Polar / rose chart: equal-angle wedges, radius proportional to value.
#[allow(clippy::too_many_arguments)]
fn render_polar(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>, grid: Rgba<u8>) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let maxv = data.iter().cloned().fold(f64::EPSILON, f64::max);
    let cx = (l + r) / 2;
    let cy = (t + b) / 2;
    let radius = ((r - l).min(b - t) / 2 - 20).max(20) as f64;
    for ring in 1..=4 {
        draw_hollow_circle_safe(img, cx, cy, (radius * ring as f64 / 4.0) as i32, grid);
    }
    let step = std::f64::consts::TAU / n as f64;
    for (i, &v) in data.iter().enumerate() {
        let a0 = -std::f64::consts::FRAC_PI_2 + i as f64 * step;
        let a1 = a0 + step * 0.9;
        let rr = v / maxv * radius;
        let steps = 10;
        let mut poly: Vec<Point<i32>> = vec![Point::new(cx, cy)];
        for k in 0..=steps {
            let a = a0 + (a1 - a0) * k as f64 / steps as f64;
            poly.push(Point::new(cx + (rr * a.cos()) as i32, cy + (rr * a.sin()) as i32));
        }
        poly.dedup();
        if poly.len() >= 3 {
            draw_polygon_mut(img, &poly, palette(i));
        }
        if let Some(c) = cats.get(i) {
            let am = (a0 + a1) / 2.0;
            draw_text_mut(img, black, cx + ((radius + 6.0) * am.cos()) as i32 - 8, cy + ((radius + 6.0) * am.sin()) as i32 - 6, PxScale::from(11.0), fnt, c);
        }
    }
}

/// `draw_hollow_circle_mut` panics on r<=0; guard it.
fn draw_hollow_circle_safe(img: &mut RgbaImage, cx: i32, cy: i32, r: i32, color: Rgba<u8>) {
    use imageproc::drawing::draw_hollow_circle_mut;
    if r > 0 {
        draw_hollow_circle_mut(img, (cx, cy), r, color);
    }
}

/// Bullet graphs: one horizontal bullet per series — qualitative range bands,
/// a measure bar, and a target tick. Series: `{name, data:[value]|value,
/// target, ranges:[r1,r2,...]}`.
#[allow(clippy::too_many_arguments)]
fn render_bullet(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>, grid: Rgba<u8>) {
    let n = series.len().max(1);
    let row_h = ((b - t) / n as i32).max(12);
    for (si, s) in series.iter().enumerate() {
        let value = s.get("value").and_then(Value::as_f64).or_else(|| series_nums(s).first().copied()).unwrap_or(0.0);
        let target = s.get("target").and_then(Value::as_f64);
        let ranges: Vec<f64> = s.get("ranges").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
        let scale_max = ranges.iter().cloned().fold(value.max(target.unwrap_or(0.0)), f64::max).max(f64::EPSILON);
        let y0 = t + si as i32 * row_h + 4;
        let bh = (row_h - 10).max(6);
        let px = |v: f64| l as f64 + v / scale_max * (r - l) as f64;
        // range bands light→dark
        let mut prev = 0.0;
        for (ri, &rmax) in ranges.iter().enumerate() {
            let shade = (220 - ri as i32 * 40).clamp(120, 220) as u8;
            draw_filled_rect_mut(img, Rect::at(px(prev) as i32, y0).of_size((px(rmax) - px(prev)).max(1.0) as u32, bh as u32), Rgba([shade, shade, shade, 255]));
            prev = rmax;
        }
        if ranges.is_empty() {
            draw_filled_rect_mut(img, Rect::at(l, y0).of_size((r - l) as u32, bh as u32), grid);
        }
        // measure bar (thinner, centered)
        let mbh = (bh / 2).max(3);
        draw_filled_rect_mut(img, Rect::at(l, y0 + (bh - mbh) / 2).of_size((px(value) - l as f64).max(1.0) as u32, mbh as u32), palette(si));
        // target tick
        if let Some(tg) = target {
            let tx = px(tg) as f32;
            draw_line_segment_mut(img, (tx, y0 as f32 - 2.0), (tx, (y0 + bh) as f32 + 2.0), Rgba([20, 20, 20, 255]));
        }
        if let Some(name) = s.get("name").and_then(Value::as_str) {
            draw_text_mut(img, black, l, y0 - 2, PxScale::from(11.0), fnt, name);
        }
    }
}

/// Pareto: bars sorted descending (left axis) + cumulative-percent line
/// (right axis 0–100%).
#[allow(clippy::too_many_arguments)]
fn render_pareto(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>, grid: Rgba<u8>) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let mut pairs: Vec<(String, f64)> = data
        .iter()
        .enumerate()
        .map(|(i, &v)| (cats.get(i).cloned().unwrap_or_else(|| format!("{}", i + 1)), v))
        .collect();
    pairs.sort_by(|a, c| c.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let total: f64 = pairs.iter().map(|(_, v)| v).sum();
    if total <= 0.0 {
        return;
    }
    let maxv = pairs.iter().map(|(_, v)| *v).fold(f64::EPSILON, f64::max);
    draw_line_segment_mut(img, (l as f32, b as f32), (r as f32, b as f32), black);
    draw_line_segment_mut(img, (l as f32, t as f32), (l as f32, b as f32), black);
    draw_line_segment_mut(img, (r as f32, t as f32), (r as f32, b as f32), black);
    let pw = (r - l) as f64;
    let ph = (b - t) as f64;
    let slot = pw / pairs.len().max(1) as f64;
    let barw = (slot * 0.7) as u32;
    // left-axis gridlines / labels
    for i in 0..=5 {
        let y = b as f32 - (i as f32 / 5.0) * ph as f32;
        draw_line_segment_mut(img, (l as f32, y), (r as f32, y), grid);
        draw_text_mut(img, black, 4, y as i32 - 6, PxScale::from(11.0), fnt, &fmt_num(maxv * i as f64 / 5.0));
        draw_text_mut(img, black, r + 2, y as i32 - 6, PxScale::from(11.0), fnt, &format!("{}%", i * 20));
    }
    let mut cum = 0.0;
    let mut prev: Option<(f32, f32)> = None;
    for (i, (name, v)) in pairs.iter().enumerate() {
        let x = l as f64 + i as f64 * slot + slot * 0.15;
        let bh = v / maxv * ph;
        draw_filled_rect_mut(img, Rect::at(x as i32, (b as f64 - bh) as i32).of_size(barw.max(1), bh.max(1.0) as u32), palette(0));
        cum += v;
        let cy = b as f32 - (cum / total) as f32 * ph as f32;
        let cxp = (x + barw as f64 / 2.0) as f32;
        draw_filled_circle_mut(img, (cxp as i32, cy as i32), 3, Rgba([0xc0, 0x3a, 0x2b, 255]));
        if let Some((px_, py_)) = prev {
            draw_line_segment_mut(img, (px_, py_), (cxp, cy), Rgba([0xc0, 0x3a, 0x2b, 255]));
        }
        prev = Some((cxp, cy));
        draw_text_mut(img, black, x as i32, b + 6, PxScale::from(11.0), fnt, name);
    }
}

/// Lollipop (stem + dot) or dot plot (`with_stem = false`).
fn render_lollipop(img: &mut RgbaImage, series: &[Value], l: i32, pw: f64, _b: i32, yp: &dyn Fn(f64) -> f32, with_stem: bool) {
    let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let slot = pw / ncat as f64;
    let base = yp(0.0);
    let nser = series.len().max(1);
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        for (ci, v) in series_nums(s).into_iter().enumerate() {
            let x = l as f64 + ci as f64 * slot + slot * 0.5 + (si as f64 - (nser as f64 - 1.0) / 2.0) * 6.0;
            let y = yp(v);
            if with_stem {
                draw_line_segment_mut(img, (x as f32, base), (x as f32, y), color);
            }
            draw_filled_circle_mut(img, (x as i32, y as i32), 5, color);
        }
    }
}

/// Gantt: one horizontal time bar per task. Each series item is a task
/// `{name, start, end, color?}` on a shared time axis.
#[allow(clippy::too_many_arguments)]
fn render_gantt(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>, grid: Rgba<u8>) {
    let tasks: Vec<(&Value, f64, f64)> = series
        .iter()
        .filter_map(|s| {
            let start = s.get("start").and_then(Value::as_f64)?;
            let end = s.get("end").and_then(Value::as_f64)?;
            Some((s, start, end))
        })
        .collect();
    if tasks.is_empty() {
        return;
    }
    let lo = tasks.iter().map(|t| t.1).fold(f64::INFINITY, f64::min);
    let hi = tasks.iter().map(|t| t.2).fold(f64::NEG_INFINITY, f64::max);
    let span = if (hi - lo).abs() < f64::EPSILON { 1.0 } else { hi - lo };
    // labels live in the left margin; bars in [l..r]
    let axis_l = l + 20;
    let map = |v: f64| axis_l as f64 + (v - lo) / span * (r - axis_l) as f64;
    let n = tasks.len();
    let row_h = ((b - t) / n as i32).max(10);
    let bh = (row_h - 6).max(4);
    // time gridlines (5)
    for i in 0..=5 {
        let v = lo + span * i as f64 / 5.0;
        let x = map(v) as f32;
        draw_line_segment_mut(img, (x, t as f32), (x, b as f32), grid);
        draw_text_mut(img, black, x as i32 - 8, b + 4, PxScale::from(11.0), fnt, &fmt_num(v));
    }
    for (i, (s, start, end)) in tasks.iter().enumerate() {
        let y = t + i as i32 * row_h + 3;
        let x0 = map(*start);
        let x1 = map(*end);
        let color = match s.get("color") {
            Some(c) => parse_color(Some(c)),
            None => palette(i),
        };
        draw_filled_rect_mut(img, Rect::at(x0 as i32, y).of_size((x1 - x0).max(1.0) as u32, bh as u32), color);
        if let Some(name) = s.get("name").and_then(Value::as_str) {
            draw_text_mut(img, black, 2, y, PxScale::from(11.0), fnt, name);
        }
    }
}

/// Sunburst / multi-ring: `rings:[[..],[..]]` innermost first (or the series'
/// data arrays as rings). Each ring's values fill the full circle.
fn render_sunburst(img: &mut RgbaImage, opts: &Value, series: &[Value], l: i32, t: i32, r: i32, b: i32) {
    let rings: Vec<Vec<f64>> = if let Some(rs) = opts.get("rings").and_then(Value::as_array) {
        rs.iter().map(|row| row.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default()).collect()
    } else {
        series.iter().map(series_nums).collect()
    };
    let nr = rings.len();
    if nr == 0 {
        return;
    }
    let cx = (l + r) / 2;
    let cy = (t + b) / 2;
    let rmax = ((r - l).min(b - t) / 2 - 10).max(20) as f64;
    let ring_w = rmax / nr as f64;
    let mut ci = 0usize;
    for (ri, ring) in rings.iter().enumerate() {
        let total: f64 = ring.iter().sum();
        if total <= 0.0 {
            continue;
        }
        let inner = ri as f64 * ring_w;
        let outer = (ri + 1) as f64 * ring_w;
        let mut angle = -std::f64::consts::FRAC_PI_2;
        for &v in ring {
            let sweep = v / total * std::f64::consts::TAU;
            let steps = (sweep / 0.1).ceil().max(2.0) as usize;
            let mut poly: Vec<Point<i32>> = Vec::with_capacity(steps * 2 + 2);
            for k in 0..=steps {
                let a = angle + sweep * k as f64 / steps as f64;
                poly.push(Point::new(cx + (outer * a.cos()) as i32, cy + (outer * a.sin()) as i32));
            }
            for k in (0..=steps).rev() {
                let a = angle + sweep * k as f64 / steps as f64;
                let ir = inner.max(0.0);
                poly.push(Point::new(cx + (ir * a.cos()) as i32, cy + (ir * a.sin()) as i32));
            }
            poly.dedup();
            if poly.len() >= 3 && poly.first() != poly.last() {
                draw_polygon_mut(img, &poly, palette(ci));
            }
            ci += 1;
            angle += sweep;
        }
    }
}

/// Floating range bars from `data:[[low,high],...]` of each series.
fn render_range(img: &mut RgbaImage, series: &[Value], l: i32, pw: f64, yp: &dyn Fn(f64) -> f32) {
    let ncat = series.iter().map(|s| series_pairs(s).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let nser = series.len().max(1);
    let slot = pw / ncat as f64;
    let barw = (slot * 0.8 / nser as f64).max(1.0);
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        for (ci, (lo, hi)) in series_pairs(s).into_iter().enumerate() {
            let x = l as f64 + ci as f64 * slot + slot * 0.1 + si as f64 * barw;
            let (y0, y1) = (yp(hi), yp(lo));
            draw_filled_rect_mut(img, Rect::at(x as i32, y0 as i32).of_size(barw as u32, (y1 - y0).max(1.0) as u32), color);
        }
    }
}

/// 100%-stacked columns: each category normalized to fill the full height.
fn render_percent_stacked(img: &mut RgbaImage, series: &[Value], l: i32, pw: f64, _b: i32, yp: &dyn Fn(f64) -> f32) {
    let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let slot = pw / ncat as f64;
    let barw = (slot * 0.8).max(1.0);
    let totals: Vec<f64> = (0..ncat)
        .map(|ci| series.iter().map(|s| series_nums(s).get(ci).copied().unwrap_or(0.0).max(0.0)).sum::<f64>().max(f64::EPSILON))
        .collect();
    let mut cum = vec![0.0f64; ncat];
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        for (ci, v) in series_nums(s).into_iter().enumerate() {
            let pct = v.max(0.0) / totals[ci] * 100.0;
            let x = l as f64 + ci as f64 * slot + slot * 0.1;
            let y0 = yp(cum[ci] + pct);
            let y1 = yp(cum[ci]);
            draw_filled_rect_mut(img, Rect::at(x as i32, y0 as i32).of_size(barw as u32, (y1 - y0).max(1.0) as u32), color);
            cum[ci] += pct;
        }
    }
}

/// Streamgraph: stacked area centered on a zero baseline (wiggle layout).
fn render_streamgraph(img: &mut RgbaImage, series: &[Value], l: i32, r: i32, b: i32, ymin: f64, ymax: f64) {
    let pw = (r - l).max(1) as f64;
    let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let xat = |i: usize| l as f64 + if ncat > 1 { i as f64 / (ncat - 1) as f64 * pw } else { pw / 2.0 };
    let yv = |v: f64| (b as f64 - (v - ymin) / (ymax - ymin) * (b as f64 - 44.0).max(1.0)) as f32;
    // baseline at -total/2 per category
    let mut cum: Vec<f64> = (0..ncat)
        .map(|ci| -series.iter().map(|s| series_nums(s).get(ci).copied().unwrap_or(0.0)).sum::<f64>() / 2.0)
        .collect();
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        let data = series_nums(s);
        let mut poly: Vec<Point<i32>> = Vec::new();
        for ci in 0..ncat {
            let v = data.get(ci).copied().unwrap_or(0.0);
            poly.push(Point::new(xat(ci) as i32, yv(cum[ci] + v) as i32));
        }
        for ci in (0..ncat).rev() {
            poly.push(Point::new(xat(ci) as i32, yv(cum[ci]) as i32));
        }
        poly.dedup();
        if poly.len() >= 3 && poly.first() != poly.last() {
            let mut fc = color;
            fc.0[3] = 180;
            draw_polygon_mut(img, &poly, fc);
        }
        for ci in 0..ncat {
            cum[ci] += data.get(ci).copied().unwrap_or(0.0);
        }
    }
}

/// Waffle chart: a 10×10 grid of cells colored by each category's share of
/// the first series' total.
fn render_waffle(img: &mut RgbaImage, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let total: f64 = data.iter().sum();
    if total <= 0.0 {
        return;
    }
    let side = ((r - l).min(b - t)).max(10);
    let cell = side / 10;
    let gap = (cell / 8).max(1);
    // cell counts per category (largest-remainder to 100)
    let mut counts: Vec<i32> = data.iter().map(|v| (v / total * 100.0).floor() as i32).collect();
    let mut assigned: i32 = counts.iter().sum();
    let mut rema: Vec<(usize, f64)> = data.iter().enumerate().map(|(i, v)| (i, (v / total * 100.0).fract())).collect();
    rema.sort_by(|a, c| c.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut ri = 0;
    while assigned < 100 && !rema.is_empty() {
        counts[rema[ri % rema.len()].0] += 1;
        assigned += 1;
        ri += 1;
    }
    let mut idx = 0usize;
    for (ci, &cnt) in counts.iter().enumerate() {
        for _ in 0..cnt {
            if idx >= 100 {
                break;
            }
            let (gx, gy) = (idx % 10, idx / 10);
            let x = l + gx as i32 * cell;
            let y = b - (gy as i32 + 1) * cell; // fill bottom-up
            draw_filled_rect_mut(img, Rect::at(x + gap, y + gap).of_size((cell - 2 * gap).max(1) as u32, (cell - 2 * gap).max(1) as u32), palette(ci));
            idx += 1;
        }
    }
    let _ = cats;
}

/// Slope chart: connect each series' first→last value across two x positions.
#[allow(clippy::too_many_arguments)]
fn render_slope(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>) {
    let vals: Vec<(&Value, f64, f64)> = series
        .iter()
        .filter_map(|s| {
            let d = series_nums(s);
            Some((s, *d.first()?, *d.last()?))
        })
        .collect();
    if vals.is_empty() {
        return;
    }
    let lo = vals.iter().flat_map(|v| [v.1, v.2]).fold(f64::INFINITY, f64::min);
    let hi = vals.iter().flat_map(|v| [v.1, v.2]).fold(f64::NEG_INFINITY, f64::max);
    let span = if (hi - lo).abs() < f64::EPSILON { 1.0 } else { hi - lo };
    let yv = |v: f64| b as f32 - ((v - lo) / span) as f32 * (b - t) as f32;
    let (x0, x1) = (l as f32 + 60.0, r as f32 - 60.0);
    draw_line_segment_mut(img, (x0, t as f32), (x0, b as f32), Rgba([210, 210, 210, 255]));
    draw_line_segment_mut(img, (x1, t as f32), (x1, b as f32), Rgba([210, 210, 210, 255]));
    for (si, (s, a, c)) in vals.iter().enumerate() {
        let color = series_color(s, si);
        draw_line_segment_mut(img, (x0, yv(*a)), (x1, yv(*c)), color);
        draw_filled_circle_mut(img, (x0 as i32, yv(*a) as i32), 4, color);
        draw_filled_circle_mut(img, (x1 as i32, yv(*c) as i32), 4, color);
        if let Some(name) = s.get("name").and_then(Value::as_str) {
            draw_text_mut(img, black, x0 as i32 - 56, yv(*a) as i32 - 6, PxScale::from(11.0), fnt, name);
        }
    }
}

/// Marimekko / mosaic: column widths ∝ each category's total, segments within
/// each column 100%-stacked by series.
#[allow(clippy::too_many_arguments)]
fn render_marimekko(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>) {
    let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let col_tot: Vec<f64> = (0..ncat)
        .map(|ci| series.iter().map(|s| series_nums(s).get(ci).copied().unwrap_or(0.0).max(0.0)).sum())
        .collect();
    let grand: f64 = col_tot.iter().sum::<f64>().max(f64::EPSILON);
    let pw = (r - l) as f64;
    let ph = (b - t) as f64;
    let mut x = l as f64;
    for ci in 0..ncat {
        let cw = col_tot[ci] / grand * pw;
        let mut y = t as f64;
        let ctot = col_tot[ci].max(f64::EPSILON);
        for (si, s) in series.iter().enumerate() {
            let v = series_nums(s).get(ci).copied().unwrap_or(0.0).max(0.0);
            let seg_h = v / ctot * ph;
            draw_filled_rect_mut(img, Rect::at(x as i32 + 1, y as i32 + 1).of_size((cw as u32).saturating_sub(2).max(1), (seg_h as u32).saturating_sub(1).max(1)), palette(si));
            y += seg_h;
        }
        if let Some(c) = cats.get(ci) {
            draw_text_mut(img, black, x as i32 + 2, b + 4, PxScale::from(10.0), fnt, c);
        }
        x += cw;
    }
}

/// Radial bar chart: each category is a concentric ring whose arc sweep is
/// proportional to its value.
#[allow(clippy::too_many_arguments)]
fn render_radial_bar(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let maxv = data.iter().cloned().fold(f64::EPSILON, f64::max);
    let cx = (l + r) / 2;
    let cy = (t + b) / 2;
    let rmax = ((r - l).min(b - t) / 2 - 16).max(20) as f64;
    let inner = rmax * 0.25;
    let band = (rmax - inner) / n as f64;
    let max_sweep = std::f64::consts::TAU * 0.75; // 270°
    for (i, &v) in data.iter().enumerate() {
        let rr = inner + (i as f64 + 0.5) * band;
        let sweep = v / maxv * max_sweep;
        let steps = (sweep / 0.1).ceil().max(2.0) as usize;
        let thick = (band * 0.7).max(2.0);
        // approximate a thick arc with overlapping filled circles
        for k in 0..=steps {
            let a = -std::f64::consts::FRAC_PI_2 + sweep * k as f64 / steps as f64;
            let x = cx + (rr * a.cos()) as i32;
            let y = cy + (rr * a.sin()) as i32;
            draw_filled_circle_mut(img, (x, y), (thick / 2.0) as i32, palette(i));
        }
        if let Some(c) = cats.get(i) {
            draw_text_mut(img, black, cx + (rr) as i32 + 2, cy - (rr) as i32 - 6, PxScale::from(10.0), fnt, c);
        }
    }
}

/// Render several chart specs and tile them into one image grid (a
/// dashboard). opts: charts => [spec,...], cols, cell_width (400),
/// cell_height (300), gap (10), background, title; path => save to any raster
/// extension or .pdf. Returns the grid image handle (+ saved path if given).
fn op_chart_grid(opts: Value) -> Result<Value> {
    let specs = opts
        .get("charts")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing charts (expected array of specs)"))?
        .clone();
    if specs.is_empty() {
        return Err(anyhow!("no charts to grid"));
    }
    let cw = opts.get("cell_width").and_then(Value::as_u64).unwrap_or(400);
    let chh = opts.get("cell_height").and_then(Value::as_u64).unwrap_or(300);
    let gap = opts.get("gap").and_then(Value::as_u64).unwrap_or(10) as u32;
    let cols = opts
        .get("cols")
        .and_then(Value::as_u64)
        .map(|c| c as usize)
        .unwrap_or_else(|| (specs.len() as f64).sqrt().ceil() as usize)
        .max(1);
    let bg = match opts.get("background") {
        Some(c) => parse_color(Some(c)),
        None => Rgba([255, 255, 255, 255]),
    };

    // Render each spec to its own handle, sizing cells uniformly.
    let mut handles = Vec::with_capacity(specs.len());
    for spec in &specs {
        let mut s = spec.clone();
        if let Some(o) = s.as_object_mut() {
            o.entry("width").or_insert(json!(cw));
            o.entry("height").or_insert(json!(chh));
        }
        let rendered = op_chart_render(s)?;
        if let Some(h) = rendered.get("handle").and_then(Value::as_u64) {
            handles.push(h);
        }
    }
    // Compose the grid.
    let imgs: Vec<image::RgbaImage> = handles.iter().map(|&h| rgba_of(h)).collect::<Result<_>>()?;
    let n = imgs.len();
    let rows = n.div_ceil(cols);
    let cell_w = imgs.iter().map(|i| i.width()).max().unwrap_or(cw as u32);
    let cell_h = imgs.iter().map(|i| i.height()).max().unwrap_or(chh as u32);
    let total_w = cols as u32 * cell_w + (cols as u32 + 1) * gap;
    let total_h = rows as u32 * cell_h + (rows as u32 + 1) * gap;
    let mut canvas = image::RgbaImage::from_pixel(total_w, total_h, bg);
    for (i, im) in imgs.iter().enumerate() {
        let (cr, cc) = (i / cols, i % cols);
        let x = gap + cc as u32 * (cell_w + gap);
        let y = gap + cr as u32 * (cell_h + gap);
        image::imageops::overlay(&mut canvas, im, x as i64, y as i64);
    }
    // Free the per-chart intermediates.
    {
        let mut map = images().lock();
        for h in &handles {
            map.remove(h);
        }
    }
    let grid = insert_image(DynamicImage::ImageRgba8(canvas));

    // Optional direct save (raster extension, or .pdf via JPEG embed).
    if let Some(path) = opts.get("path").and_then(Value::as_str) {
        let ext = ext_of(path);
        let result = if ext == "pdf" {
            let (jpeg, w, h) = with_image(grid, |img| {
                use image::GenericImageView;
                let (w, h) = img.dimensions();
                let mut buf = std::io::Cursor::new(Vec::new());
                img.to_rgb8().write_to(&mut buf, image::ImageFormat::Jpeg).map_err(|e| anyhow!("encode jpeg: {e}"))?;
                Ok((buf.into_inner(), w, h))
            })?;
            std::fs::write(path, pdf_with_jpeg(&jpeg, w, h)).map_err(|e| anyhow!("write {path}: {e}"))
        } else {
            with_image(grid, |img| img.save(path).map_err(|e| anyhow!("save {path}: {e}")))
        };
        result?;
        return Ok(json!({"ok": true, "handle": grid, "path": path, "charts": n}));
    }
    with_image(grid, |img| Ok(info_json_chart(grid, img, n)))
}

/// Image info plus a chart count, for chart_grid.
fn info_json_chart(handle: u64, img: &DynamicImage, charts: usize) -> Value {
    json!({"handle": handle, "width": img.width(), "height": img.height(), "charts": charts})
}

/// Calendar heatmap (GitHub-style): `values` (or the first series' data) laid
/// out 7 rows × N columns, colored white→green by value.
fn render_calendar(img: &mut RgbaImage, opts: &Value, series: &[Value], l: i32, t: i32, r: i32, b: i32) {
    let values: Vec<f64> = opts
        .get("values")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_else(|| series.first().map(series_nums).unwrap_or_default());
    let n = values.len();
    if n == 0 {
        return;
    }
    let (lo, hi) = values.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, c), &v| (a.min(v), c.max(v)));
    let span = if (hi - lo).abs() < f64::EPSILON { 1.0 } else { hi - lo };
    let cols = opts.get("columns").and_then(Value::as_u64).map(|c| c as usize).unwrap_or(n.div_ceil(7)).max(1);
    let cell = (((r - l) / cols as i32).min((b - t) / 7)).max(3);
    for (i, &v) in values.iter().enumerate() {
        let (col, row) = (i / 7, i % 7);
        let frac = (v - lo) / span;
        // white → GitHub green (#216e39)
        let color = Rgba([
            (255.0 - frac * (255.0 - 0x21 as f64)) as u8,
            (255.0 - frac * (255.0 - 0x6e as f64)) as u8,
            (255.0 - frac * (255.0 - 0x39 as f64)) as u8,
            255,
        ]);
        let x = l + col as i32 * cell;
        let y = t + row as i32 * cell;
        draw_filled_rect_mut(img, Rect::at(x + 1, y + 1).of_size((cell - 2).max(1) as u32, (cell - 2).max(1) as u32), color);
    }
}

/// Parallel coordinates: one vertical axis per dimension, each series a
/// polyline crossing all axes (each axis independently normalized).
#[allow(clippy::too_many_arguments)]
fn render_parallel(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>, grid: Rgba<u8>) {
    let ndim = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0).max(cats.len());
    if ndim < 2 {
        return;
    }
    let mut dmin = vec![f64::INFINITY; ndim];
    let mut dmax = vec![f64::NEG_INFINITY; ndim];
    for s in series {
        for (d, v) in series_nums(s).into_iter().enumerate() {
            dmin[d] = dmin[d].min(v);
            dmax[d] = dmax[d].max(v);
        }
    }
    let xat = |d: usize| l as f64 + d as f64 / (ndim - 1) as f64 * (r - l) as f64;
    let yat = |d: usize, v: f64| {
        let span = (dmax[d] - dmin[d]).abs().max(1e-9);
        b as f64 - (v - dmin[d]) / span * (b - t) as f64
    };
    for d in 0..ndim {
        let x = xat(d) as f32;
        draw_line_segment_mut(img, (x, t as f32), (x, b as f32), grid);
        if let Some(c) = cats.get(d) {
            draw_text_mut(img, black, x as i32 - 8, b + 4, PxScale::from(11.0), fnt, c);
        }
    }
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        let data = series_nums(s);
        for d in 1..data.len() {
            draw_line_segment_mut(img, (xat(d - 1) as f32, yat(d - 1, data[d - 1]) as f32), (xat(d) as f32, yat(d, data[d]) as f32), color);
        }
    }
}

// ── hex grid math (flat-top, redblobgames conventions) ───────────────────────

fn pixel_to_axial(px: f64, py: f64, size: f64) -> (f64, f64) {
    let q = (2.0 / 3.0 * px) / size;
    let r = (-1.0 / 3.0 * px + 3f64.sqrt() / 3.0 * py) / size;
    (q, r)
}

fn axial_round(q: f64, r: f64) -> (i32, i32) {
    let (x, z) = (q, r);
    let y = -x - z;
    let (mut rx, mut ry, mut rz) = (x.round(), y.round(), z.round());
    let (dx, dy, dz) = ((rx - x).abs(), (ry - y).abs(), (rz - z).abs());
    if dx > dy && dx > dz {
        rx = -ry - rz;
    } else if dy > dz {
        ry = -rx - rz;
    } else {
        rz = -rx - ry;
    }
    let _ = ry;
    (rx as i32, rz as i32)
}

fn axial_to_pixel(q: i32, r: i32, size: f64) -> (f64, f64) {
    let px = size * 1.5 * q as f64;
    let py = size * 3f64.sqrt() * (r as f64 + q as f64 / 2.0);
    (px, py)
}

/// Hexbin: bin scatter points (`data:[[x,y],...]` across series) into a
/// flat-top hex grid, colored white→blue by count. opts: radius (px, 16).
fn render_hexbin(img: &mut RgbaImage, opts: &Value, series: &[Value], l: i32, t: i32, r: i32, b: i32) {
    let pts: Vec<(f64, f64)> = series.iter().flat_map(series_points).collect();
    if pts.is_empty() {
        return;
    }
    let (mut xmn, mut xmx, mut ymn, mut ymx) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
    for &(x, y) in &pts {
        xmn = xmn.min(x);
        xmx = xmx.max(x);
        ymn = ymn.min(y);
        ymx = ymx.max(y);
    }
    let xspan = (xmx - xmn).abs().max(1e-9);
    let yspan = (ymx - ymn).abs().max(1e-9);
    let size = opts.get("radius").and_then(Value::as_f64).unwrap_or(16.0).max(4.0);
    let to_px = |x: f64, y: f64| ((x - xmn) / xspan * (r - l) as f64, (ymx - y) / yspan * (b - t) as f64);
    let mut bins: std::collections::HashMap<(i32, i32), u32> = std::collections::HashMap::new();
    for &(x, y) in &pts {
        let (px, py) = to_px(x, y);
        let (q, rr) = pixel_to_axial(px, py, size);
        *bins.entry(axial_round(q, rr)).or_insert(0) += 1;
    }
    let maxc = bins.values().copied().max().unwrap_or(1) as f64;
    for (&(q, rr), &cnt) in &bins {
        let (cx, cy) = axial_to_pixel(q, rr, size);
        let (cx, cy) = (l as f64 + cx, t as f64 + cy);
        let frac = cnt as f64 / maxc;
        let color = Rgba([(255.0 - frac * 187.0) as u8, (255.0 - frac * 141.0) as u8, 255, 255]);
        let poly: Vec<Point<i32>> = (0..6)
            .map(|i| {
                let a = std::f64::consts::PI / 3.0 * i as f64; // flat-top: first vertex at 0°
                Point::new((cx + size * a.cos()) as i32, (cy + size * a.sin()) as i32)
            })
            .collect();
        let mut p = poly;
        p.dedup();
        if p.len() >= 3 {
            draw_polygon_mut(img, &p, color);
        }
    }
}
