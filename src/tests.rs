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
    // A per-call atomic counter makes every temp path unique even when two tests
    // pass the same `name`, so concurrent tests never collide on a shared file.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "stryke-office-test-{}-{n}-{name}",
        std::process::id()
    ));
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
fn sheet_union_aligns_by_name() {
    // two files with the SAME logical columns in DIFFERENT order
    let a = tmp("ua.xlsx");
    let b = tmp("ub.xlsx");
    call(
        office__sheet_write,
        &format!(r#"{{"path":"{a}","sheets":[{{"name":"S","rows":[["name","age"],["x",1]]}}]}}"#),
    );
    call(
        office__sheet_write,
        &format!(r#"{{"path":"{b}","sheets":[{{"name":"S","rows":[["age","name"],[2,"y"]]}}]}}"#),
    );

    let out = tmp("u_out.xlsx");
    let r = call(
        office__sheet_union,
        &format!(r#"{{"inputs":["{a}","{b}"],"output":"{out}"}}"#),
    );
    assert_eq!(r["rows"], 2, "two data rows: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = &rd["sheets"][0]["rows"];
    assert_eq!(
        rows[0][0], "name",
        "union header order from first file: {rd}"
    );
    assert_eq!(rows[0][1], "age");
    // second file's columns were swapped, but values land under the right names
    assert_eq!(rows[2][0], "y", "row2 name aligned: {rd}");
    assert_eq!(rows[2][1], 2.0, "row2 age aligned: {rd}");

    for f in [&a, &b, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_merge_workbooks_and_rows() {
    // two xlsx, distinct sheet names
    let a = tmp("wbA.xlsx");
    let b = tmp("wbB.xlsx");
    let wa = call(
        office__sheet_write,
        &format!(r#"{{"path":"{a}","sheets":[{{"name":"S1","rows":[["a","b"],[1,2]]}}]}}"#),
    );
    assert_eq!(wa["ok"], true, "write A: {wa}");
    let wb = call(
        office__sheet_write,
        &format!(r#"{{"path":"{b}","sheets":[{{"name":"S2","rows":[["c"],[3]]}}]}}"#),
    );
    assert_eq!(wb["ok"], true, "write B: {wb}");

    // sheets mode -> one workbook with both sheets
    let merged = tmp("wbM.xlsx");
    let m = call(
        office__sheet_merge,
        &format!(r#"{{"inputs":["{a}","{b}"],"output":"{merged}"}}"#),
    );
    assert_eq!(m["sheets"], 2, "two sheets merged: {m}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{merged}"}}"#));
    let names: Vec<&str> = rd["sheets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"S1") && names.contains(&"S2"),
        "both sheet names: {rd}"
    );

    // rows mode merging two csv into one csv (stacked rows + conversion path)
    let ca = tmp("cA.csv");
    let cb = tmp("cB.csv");
    std::fs::write(&ca, "x,y\n1,2\n").unwrap();
    std::fs::write(&cb, "3,4\n").unwrap();
    let cm = tmp("cM.csv");
    let r = call(
        office__sheet_merge,
        &format!(r#"{{"inputs":["{ca}","{cb}"],"output":"{cm}","mode":"rows"}}"#),
    );
    assert_eq!(r["ok"], true, "csv rows merge: {r}");
    let rc = call(office__sheet_read, &format!(r#"{{"path":"{cm}"}}"#));
    assert_eq!(
        rc["sheets"][0]["rows"].as_array().unwrap().len(),
        3,
        "3 stacked rows: {rc}"
    );

    for f in [&a, &b, &merged, &ca, &cb, &cm] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_stats_per_column_aggregates() {
    let path = tmp("stats.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"Data","rows":[
                ["name","qty","note"],
                ["a",10,"x"],
                ["b",20,""],
                ["c",30,"y"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(office__sheet_stats, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(s["rows"], 3, "3 data rows: {s}");
    let cols = s["columns"].as_array().unwrap();
    assert_eq!(cols[0]["name"], "name", "header name");
    assert_eq!(cols[0]["numeric"], 0, "text column not numeric");
    // qty column: numeric aggregates
    assert_eq!(cols[1]["name"], "qty");
    assert_eq!(cols[1]["numeric"], 3, "3 numeric: {s}");
    assert_eq!(cols[1]["sum"], 60.0, "sum: {s}");
    assert_eq!(cols[1]["min"], 10.0, "min: {s}");
    assert_eq!(cols[1]["max"], 30.0, "max: {s}");
    assert_eq!(cols[1]["mean"], 20.0, "mean: {s}");
    // note column: one blank
    assert_eq!(cols[2]["name"], "note");
    assert_eq!(cols[2]["count"], 2, "two non-blank notes: {s}");
    assert_eq!(cols[2]["blanks"], 1, "one blank: {s}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_describe_numeric_summary() {
    let path = tmp("describe.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["label","v"],
                ["a",1],
                ["b",2],
                ["c",3],
                ["d",4]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(office__sheet_describe, &format!(r#"{{"path":"{path}"}}"#));
    let cols = s["columns"].as_array().unwrap();
    // Only the numeric "v" column is reported (text "label" skipped).
    assert_eq!(cols.len(), 1, "one numeric column: {s}");
    let v = &cols[0];
    assert_eq!(v["name"], "v", "column name: {s}");
    assert_eq!(v["count"], 4, "count: {s}");
    assert_eq!(v["mean"], 2.5, "mean: {s}");
    assert_eq!(v["min"], 1.0, "min: {s}");
    assert_eq!(v["max"], 4.0, "max: {s}");
    assert_eq!(v["p25"], 1.75, "p25 linear interp: {s}");
    assert_eq!(v["p50"], 2.5, "median: {s}");
    assert_eq!(v["p75"], 3.25, "p75 linear interp: {s}");
    // sample std (ddof=1) of [1,2,3,4] = sqrt(5/3) ≈ 1.2909944
    let std = v["std"].as_f64().unwrap();
    assert!((std - 1.290_994_4).abs() < 1e-6, "sample std: {s}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_dtypes_infers_column_types() {
    let path = tmp("dtypes.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["name","qty","price","mixed"],
                ["a",1,1.5,"x"],
                ["b",2,2.5,7],
                ["c",3,3.5,""]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(office__sheet_dtypes, &format!(r#"{{"path":"{path}"}}"#));
    let cols = r["columns"].as_array().unwrap();
    assert_eq!(cols.len(), 4, "four columns: {r}");
    assert_eq!(cols[0]["name"], "name", "col0 name: {r}");
    assert_eq!(cols[0]["type"], "string", "name -> string: {r}");
    assert_eq!(cols[1]["type"], "integer", "qty -> integer: {r}");
    assert_eq!(cols[2]["type"], "float", "price -> float: {r}");
    // "mixed" has a string, an int, and a blank → mixed
    assert_eq!(cols[3]["type"], "mixed", "mixed -> mixed: {r}");
    assert_eq!(cols[3]["counts"]["blank"], 1, "one blank in mixed: {r}");
    assert_eq!(cols[3]["counts"]["string"], 1, "one string in mixed: {r}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_profile_per_column() {
    let path = tmp("profile.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["age","city"],
                [30,"NYC"],
                [40,"NYC"],
                [50,"LA"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let p = call(office__sheet_profile, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(p["rows"].as_u64().unwrap(), 3, "three data rows: {p}");
    let cols = p["columns"].as_array().unwrap();
    // numeric column
    assert_eq!(cols[0]["name"], "age", "col name: {p}");
    assert_eq!(cols[0]["type"], "numeric", "age numeric: {p}");
    assert_eq!(cols[0]["min"].as_f64().unwrap(), 30.0, "age min: {p}");
    assert_eq!(cols[0]["mean"].as_f64().unwrap(), 40.0, "age mean: {p}");
    // text column with a top value
    assert_eq!(cols[1]["type"], "text", "city text: {p}");
    assert_eq!(cols[1]["distinct"].as_u64().unwrap(), 2, "two cities: {p}");
    assert_eq!(cols[1]["top"], "NYC", "top city: {p}");
    assert_eq!(cols[1]["top_count"].as_u64().unwrap(), 2, "NYC count: {p}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_mode_most_frequent() {
    let path = tmp("mode.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["color","n"],
                ["red",1],
                ["blue",1],
                ["red",2],
                ["red",3]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(office__sheet_mode, &format!(r#"{{"path":"{path}"}}"#));
    let cols = r["columns"].as_array().unwrap();
    assert_eq!(cols[0]["name"], "color", "col0 name: {r}");
    assert_eq!(cols[0]["mode"], "red", "most frequent color: {r}");
    assert_eq!(cols[0]["count"], 3, "red appears 3x: {r}");
    // n column: 1 appears twice -> mode 1
    assert_eq!(cols[1]["mode"].as_f64().unwrap(), 1.0, "n mode 1: {r}");
    assert_eq!(cols[1]["count"], 2, "1 appears 2x: {r}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_nunique_cardinality() {
    let path = tmp("nunique.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["color","n"],
                ["red",1],
                ["blue",1],
                ["red",2],
                ["",5]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(office__sheet_nunique, &format!(r#"{{"path":"{path}"}}"#));
    let cols = r["columns"].as_array().unwrap();
    // color: red, blue (blank dropped) = 2 ; n: 1, 2, 5 = 3
    assert_eq!(cols[0]["name"], "color", "col0 name: {r}");
    assert_eq!(cols[0]["nunique"], 2, "two distinct colors: {r}");
    assert_eq!(cols[1]["nunique"], 3, "three distinct n: {r}");

    // dropna=false counts the blank color as its own value -> 3
    let rk = call(
        office__sheet_nunique,
        &format!(r#"{{"path":"{path}","dropna":false}}"#),
    );
    assert_eq!(rk["columns"][0]["nunique"], 3, "blank counted: {rk}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_entropy_diversity() {
    // uniform 4-way distribution -> entropy 2.0 bits, normalized 1.0
    let path = tmp("entropy.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["c"],["a"],["b"],["c"],["d"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(
        office__sheet_entropy,
        &format!(r#"{{"path":"{path}","column":"c","decimals":6}}"#),
    );
    assert_eq!(s["distinct"].as_u64().unwrap(), 4, "four distinct: {s}");
    assert!(
        (s["entropy"].as_f64().unwrap() - 2.0).abs() < 1e-9,
        "uniform 4 -> 2 bits: {s}"
    );
    assert!(
        (s["normalized"].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "uniform -> normalized 1: {s}"
    );

    // constant column -> entropy 0
    let path2 = tmp("entropy2.xlsx");
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path2}","sheets":[{{"name":"D","rows":[["c"],["x"],["x"],["x"]]}}]}}"#
        ),
    );
    let s2 = call(
        office__sheet_entropy,
        &format!(r#"{{"path":"{path2}","column":"c"}}"#),
    );
    assert_eq!(
        s2["entropy"].as_f64().unwrap(),
        0.0,
        "constant -> 0 entropy: {s2}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&path2).ok();
}

#[test]
fn sheet_gini_inequality() {
    // equal values -> Gini 0
    let path = tmp("gini.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[5],[5],[5],[5]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(
        office__sheet_gini,
        &format!(r#"{{"path":"{path}","column":"v","decimals":6}}"#),
    );
    assert_eq!(s["gini"].as_f64().unwrap(), 0.0, "equal -> Gini 0: {s}");

    // maximal concentration [0,0,0,4] -> Gini (n-1)/n = 0.75
    let path2 = tmp("gini2.xlsx");
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path2}","sheets":[{{"name":"D","rows":[["v"],[0],[0],[0],[4]]}}]}}"#
        ),
    );
    let s2 = call(
        office__sheet_gini,
        &format!(r#"{{"path":"{path2}","column":"v","decimals":6}}"#),
    );
    assert!(
        (s2["gini"].as_f64().unwrap() - 0.75).abs() < 1e-9,
        "concentrated -> 0.75: {s2}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&path2).ok();
}

#[test]
fn sheet_count_completeness() {
    let path = tmp("count.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["a","b"],
                ["x",1],
                ["",2],
                ["y",""]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(office__sheet_count, &format!(r#"{{"path":"{path}"}}"#));
    let cols = r["columns"].as_array().unwrap();
    // a: x,y filled (one blank) -> 2/1 ; b: 1,2 filled (one blank) -> 2/1
    assert_eq!(cols[0]["count"], 2, "a filled: {r}");
    assert_eq!(cols[0]["blank"], 1, "a blank: {r}");
    assert_eq!(cols[1]["count"], 2, "b filled: {r}");
    assert_eq!(cols[1]["blank"], 1, "b blank: {r}");
    assert_eq!(r["rows"], 3, "three data rows: {r}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_quantile_arbitrary_percentile() {
    let path = tmp("quantile.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[1],[2],[3],[4]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let med = call(
        office__sheet_quantile,
        &format!(r#"{{"path":"{path}","column":"v","q":0.5}}"#),
    );
    assert_eq!(med["count"], 4, "four values: {med}");
    assert!(
        (med["value"].as_f64().unwrap() - 2.5).abs() < 1e-9,
        "median 2.5: {med}"
    );

    let p0 = call(
        office__sheet_quantile,
        &format!(r#"{{"path":"{path}","column":"v","q":0.0}}"#),
    );
    assert!(
        (p0["value"].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "min: {p0}"
    );
    let p100 = call(
        office__sheet_quantile,
        &format!(r#"{{"path":"{path}","column":"v","q":1.0}}"#),
    );
    assert!(
        (p100["value"].as_f64().unwrap() - 4.0).abs() < 1e-9,
        "max: {p100}"
    );
    // p25 linear interp = 1.75
    let p25 = call(
        office__sheet_quantile,
        &format!(r#"{{"path":"{path}","column":"v","q":0.25}}"#),
    );
    assert!(
        (p25["value"].as_f64().unwrap() - 1.75).abs() < 1e-9,
        "p25 interp: {p25}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_moments_skew_kurtosis() {
    let path = tmp("moments.xlsx");
    // symmetric data -> skewness ~ 0
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"M","rows":[["v"],[1],[2],[3],[4],[5]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(
        office__sheet_moments,
        &format!(r#"{{"path":"{path}","column":"v","decimals":6}}"#),
    );
    assert_eq!(s["n"].as_u64().unwrap(), 5, "n=5: {s}");
    assert_eq!(s["mean"].as_f64().unwrap(), 3.0, "mean 3: {s}");
    assert_eq!(
        s["variance"].as_f64().unwrap(),
        2.5,
        "sample variance 2.5: {s}"
    );
    assert!(
        s["skewness"].as_f64().unwrap().abs() < 1e-6,
        "symmetric skew ~0: {s}"
    );

    // right-skewed data -> positive skewness
    let path2 = tmp("moments_skew.xlsx");
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path2}","sheets":[{{"name":"M","rows":[["v"],[1],[1],[1],[2],[10]]}}]}}"#
        ),
    );
    let s2 = call(
        office__sheet_moments,
        &format!(r#"{{"path":"{path2}","column":"v"}}"#),
    );
    assert!(
        s2["skewness"].as_f64().unwrap() > 0.5,
        "right-skewed positive: {s2}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&path2).ok();
}

#[test]
fn sheet_means_three_kinds() {
    let path = tmp("means.xlsx");
    // 1,2,4: arithmetic 7/3≈2.3333, geometric (1*2*4)^(1/3)=2, harmonic 3/(1.75)≈1.7143
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[1],[2],[4]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(
        office__sheet_means,
        &format!(r#"{{"path":"{path}","column":"v","decimals":6}}"#),
    );
    assert!(
        (s["arithmetic"].as_f64().unwrap() - 2.333333).abs() < 1e-5,
        "arithmetic: {s}"
    );
    assert!(
        (s["geometric"].as_f64().unwrap() - 2.0).abs() < 1e-9,
        "geometric: {s}"
    );
    assert!(
        (s["harmonic"].as_f64().unwrap() - 1.714286).abs() < 1e-5,
        "harmonic: {s}"
    );

    // a zero value -> harmonic null, and a negative -> geometric null
    let path2 = tmp("means2.xlsx");
    call(
        office__sheet_write,
        &format!(r#"{{"path":"{path2}","sheets":[{{"name":"D","rows":[["v"],[0],[-1],[2]]}}]}}"#),
    );
    let s2 = call(
        office__sheet_means,
        &format!(r#"{{"path":"{path2}","column":"v"}}"#),
    );
    assert!(
        s2["geometric"].is_null(),
        "geometric null with non-positive: {s2}"
    );
    assert!(s2["harmonic"].is_null(), "harmonic null with zero: {s2}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&path2).ok();
}

#[test]
fn sheet_mad_robust_spread() {
    let path = tmp("mad.xlsx");
    // 1,2,3,4,5: median 3, abs devs [2,1,0,1,2], median dev = 1
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[1],[2],[3],[4],[5]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(
        office__sheet_mad,
        &format!(r#"{{"path":"{path}","column":"v","decimals":6}}"#),
    );
    assert_eq!(s["median"].as_f64().unwrap(), 3.0, "median 3: {s}");
    assert_eq!(s["mad"].as_f64().unwrap(), 1.0, "mad 1: {s}");

    // scaled by 1.4826
    let ss = call(
        office__sheet_mad,
        &format!(r#"{{"path":"{path}","column":"v","scaled":true,"decimals":4}}"#),
    );
    assert!(
        (ss["mad"].as_f64().unwrap() - 1.4826).abs() < 1e-3,
        "scaled mad: {ss}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_npv_discount() {
    let path = tmp("npv.xlsx");
    // cashflows 100, 200, 300 at 10% — Excel NPV (first discounted 1 period)
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"C","rows":[["cf"],[100],[200],[300]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(
        office__sheet_npv,
        &format!(r#"{{"path":"{path}","column":"cf","rate":0.1,"decimals":4}}"#),
    );
    // 100/1.1 + 200/1.21 + 300/1.331 = 90.9091 + 165.2893 + 225.3944 = 481.5928
    assert!(
        (s["npv"].as_f64().unwrap() - 481.5928).abs() < 1e-3,
        "npv excel convention: {s}"
    );
    assert_eq!(s["n"].as_u64().unwrap(), 3, "three cashflows: {s}");

    // start=0: first cashflow undiscounted -> 100 + 200/1.1 + 300/1.21
    let s0 = call(
        office__sheet_npv,
        &format!(r#"{{"path":"{path}","column":"cf","rate":0.1,"start":0,"decimals":4}}"#),
    );
    assert!(
        (s0["npv"].as_f64().unwrap() - 529.7521).abs() < 1e-3,
        "npv start=0: {s0}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_sumproduct_weighted() {
    let path = tmp("sumprod.xlsx");
    // price * qty: 2*3 + 4*5 + 6*1 = 6 + 20 + 6 = 32
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["price","qty"],[2,3],[4,5],[6,1]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(
        office__sheet_sumproduct,
        &format!(r#"{{"path":"{path}","columns":["price","qty"]}}"#),
    );
    assert_eq!(
        s["sumproduct"].as_f64().unwrap(),
        32.0,
        "weighted total 32: {s}"
    );
    assert_eq!(s["n"].as_u64().unwrap(), 3, "three rows: {s}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_irr_internal_rate() {
    let path = tmp("irr.xlsx");
    // -1000 then 600, 600: IRR is ~13.07%
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"C","rows":[["cf"],[-1000],[600],[600]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(
        office__sheet_irr,
        &format!(r#"{{"path":"{path}","column":"cf"}}"#),
    );
    let irr = s["irr"].as_f64().unwrap();
    assert!((irr - 0.130662).abs() < 1e-4, "irr ~ 0.1307: {s}");

    // cross-check: NPV at the found IRR is ~0
    let chk = call(
        office__sheet_npv,
        &format!(r#"{{"path":"{path}","column":"cf","rate":{irr},"start":0}}"#),
    );
    assert!(
        chk["npv"].as_f64().unwrap().abs() < 1e-2,
        "NPV at IRR ~ 0: {chk}"
    );

    // no sign change -> error
    let path2 = tmp("irr_bad.xlsx");
    call(
        office__sheet_write,
        &format!(r#"{{"path":"{path2}","sheets":[{{"name":"C","rows":[["cf"],[100],[200]]}}]}}"#),
    );
    let bad = call(
        office__sheet_irr,
        &format!(r#"{{"path":"{path2}","column":"cf"}}"#),
    );
    assert!(bad.get("error").is_some(), "no sign change errors: {bad}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&path2).ok();
}

#[test]
fn sheet_cagr_growth_rate() {
    let path = tmp("cagr.xlsx");
    // 100 -> 200 over 2 intervals (3 values) -> CAGR = sqrt(2)-1 ≈ 0.41421
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[100],[150],[200]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(
        office__sheet_cagr,
        &format!(r#"{{"path":"{path}","column":"v","decimals":6}}"#),
    );
    assert_eq!(s["start"].as_f64().unwrap(), 100.0, "start: {s}");
    assert_eq!(s["end"].as_f64().unwrap(), 200.0, "end: {s}");
    assert_eq!(s["periods"].as_f64().unwrap(), 2.0, "two intervals: {s}");
    assert!(
        (s["cagr"].as_f64().unwrap() - 0.414214).abs() < 1e-5,
        "cagr sqrt(2)-1: {s}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_drawdown_max() {
    let path = tmp("drawdown.xlsx");
    // 100,120,90,110,80: peak 120, worst trough 80 -> dd = 40/120 = 0.3333
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[100],[120],[90],[110],[80]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(
        office__sheet_drawdown,
        &format!(r#"{{"path":"{path}","column":"v","decimals":6}}"#),
    );
    assert!(
        (s["max_drawdown"].as_f64().unwrap() - 0.333333).abs() < 1e-5,
        "max dd ~0.333: {s}"
    );
    assert_eq!(s["peak"].as_f64().unwrap(), 120.0, "peak 120: {s}");
    assert_eq!(s["trough"].as_f64().unwrap(), 80.0, "trough 80: {s}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_amortize_schedule() {
    // $1000 at 1%/period over 12 periods
    let out = tmp("amort.xlsx");
    let r = call(
        office__sheet_amortize,
        &format!(r#"{{"rate":0.01,"nper":12,"pv":1000,"output":"{out}"}}"#),
    );
    assert_eq!(r["periods"].as_u64().unwrap(), 12, "12 periods: {r}");
    // PMT = 1000*0.01 / (1 - 1.01^-12) = 88.8488...
    assert!(
        (r["payment"].as_f64().unwrap() - 88.85).abs() < 0.01,
        "level payment ~88.85: {r}"
    );

    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 13, "header + 12 rows");
    assert_eq!(rows[0][0], "period", "header: {rd}");
    // first period interest = 1000 * 0.01 = 10
    assert_eq!(
        rows[1][3].as_f64().unwrap(),
        10.0,
        "first interest = 10: {rd}"
    );
    // final balance is 0 (drift absorbed)
    assert_eq!(rows[12][4].as_f64().unwrap(), 0.0, "final balance 0: {rd}");

    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_autocorr_acf() {
    // a perfectly periodic series: ACF at lag 0 is 1; an alternating series is
    // anti-correlated at lag 1.
    let path = tmp("acf.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[["v"],[1],[-1],[1],[-1],[1],[-1]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(
        office__sheet_autocorr,
        &format!(r#"{{"path":"{path}","column":"v","lags":2,"decimals":6}}"#),
    );
    let acf = s["acf"].as_array().unwrap();
    assert_eq!(acf.len(), 3, "lags 0..=2: {s}");
    assert_eq!(acf[0]["lag"], 0, "first is lag 0: {s}");
    assert!(
        (acf[0]["value"].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "r0 = 1: {s}"
    );
    // alternating sign -> strong negative lag-1 autocorrelation
    assert!(
        acf[1]["value"].as_f64().unwrap() < -0.5,
        "lag1 anti-correlated: {s}"
    );
    // lag-2 (same sign) -> positive
    assert!(
        acf[2]["value"].as_f64().unwrap() > 0.4,
        "lag2 positive: {s}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_agg_scalar() {
    let path = tmp("agg.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[10],[20],[30]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let g = |a: &str| {
        call(
            office__sheet_agg,
            &format!(r#"{{"path":"{path}","column":"v","agg":"{a}"}}"#),
        )
    };
    assert_eq!(g("sum")["value"].as_f64().unwrap(), 60.0, "sum");
    assert_eq!(g("mean")["value"].as_f64().unwrap(), 20.0, "mean");
    assert_eq!(g("min")["value"].as_f64().unwrap(), 10.0, "min");
    assert_eq!(g("max")["value"].as_f64().unwrap(), 30.0, "max");
    assert_eq!(g("count")["value"].as_u64().unwrap(), 3, "count");
    assert_eq!(g("median")["value"].as_f64().unwrap(), 20.0, "median");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_sparkline_blocks() {
    let path = tmp("spark.xlsx");
    // 1..8 maps linearly onto the eight block levels
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[1],[2],[3],[4],[5],[6],[7],[8]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_sparkline,
        &format!(r#"{{"path":"{path}","column":"v"}}"#),
    );
    assert_eq!(r["count"], 8, "eight values: {r}");
    assert_eq!(r["sparkline"], "▁▂▃▄▅▆▇█", "full ramp of blocks: {r}");
    assert_eq!(r["min"].as_f64().unwrap(), 1.0, "min: {r}");
    assert_eq!(r["max"].as_f64().unwrap(), 8.0, "max: {r}");

    // a flat column renders at the lowest block
    let pf = tmp("spark_flat.xlsx");
    call(
        office__sheet_write,
        &format!(r#"{{"path":"{pf}","sheets":[{{"name":"D","rows":[["v"],[5],[5],[5]]}}]}}"#),
    );
    let rf = call(
        office__sheet_sparkline,
        &format!(r#"{{"path":"{pf}","column":"v"}}"#),
    );
    assert_eq!(rf["sparkline"], "▁▁▁", "flat -> lowest block: {rf}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&pf).ok();
}

#[test]
fn sheet_argmax_locates_extreme() {
    let path = tmp("argmax.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["name","sales"],
                ["a",30],
                ["b",90],
                ["c",10]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // max sales -> row 1 (b, 90)
    let r = call(
        office__sheet_argmax,
        &format!(r#"{{"path":"{path}","column":"sales","label":"name"}}"#),
    );
    assert_eq!(r["row"], 1, "max at data row 1: {r}");
    assert_eq!(r["value"].as_f64().unwrap(), 90.0, "max value: {r}");
    assert_eq!(r["label"], "b", "label of the max row: {r}");

    // min sales -> row 2 (c, 10)
    let rm = call(
        office__sheet_argmax,
        &format!(r#"{{"path":"{path}","column":"sales","min":true,"label":"name"}}"#),
    );
    assert_eq!(rm["row"], 2, "min at data row 2: {rm}");
    assert_eq!(rm["label"], "c", "label of the min row: {rm}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_corr_pearson_matrix() {
    let path = tmp("corr.xlsx");
    // y = 2x (perfect +), z = -x (perfect -).
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"C","rows":[
                ["x","y","z"],
                [1,2,3],
                [2,4,2],
                [3,6,1]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(office__sheet_corr, &format!(r#"{{"path":"{path}"}}"#));
    let names = s["columns"].as_array().unwrap();
    assert_eq!(names.len(), 3, "three numeric columns: {s}");
    assert_eq!(names[0], "x");
    let m = &s["matrix"];
    // diagonal
    assert_eq!(m[0][0], 1.0, "diag x: {s}");
    assert_eq!(m[1][1], 1.0, "diag y: {s}");
    // x vs y perfectly correlated, x vs z perfectly anti-correlated
    assert!(
        (m[0][1].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "x~y=+1: {s}"
    );
    assert!(
        (m[0][2].as_f64().unwrap() + 1.0).abs() < 1e-9,
        "x~z=-1: {s}"
    );
    // symmetric
    assert_eq!(m[1][0], m[0][1], "symmetric: {s}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_corr_spearman_monotonic() {
    // y = x^3 is monotonic but non-linear: Spearman = 1 while Pearson < 1.
    let path = tmp("corr_sp.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"C","rows":[
                ["x","y"],
                [1,1],
                [2,8],
                [3,27],
                [4,64],
                [5,125]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let sp = call(
        office__sheet_corr,
        &format!(r#"{{"path":"{path}","method":"spearman"}}"#),
    );
    assert!(
        (sp["matrix"][0][1].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "spearman monotonic = 1: {sp}"
    );

    let pe = call(office__sheet_corr, &format!(r#"{{"path":"{path}"}}"#));
    assert!(
        pe["matrix"][0][1].as_f64().unwrap() < 0.98,
        "pearson < 1 for x^3: {pe}"
    );

    // Kendall's tau-b is also 1 for a strictly increasing (concordant) relation
    let kd = call(
        office__sheet_corr,
        &format!(r#"{{"path":"{path}","method":"kendall"}}"#),
    );
    assert!(
        (kd["matrix"][0][1].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "kendall concordant = 1: {kd}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_regress_ols() {
    // y = 3x + 2 exactly -> slope 3, intercept 2, r2 1
    let path = tmp("regress.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"R","rows":[
                ["x","y"],
                [1,5],
                [2,8],
                [3,11],
                [4,14]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("regress_out.xlsx");
    let s = call(
        office__sheet_regress,
        &format!(r#"{{"path":"{path}","x":"x","y":"y","output":"{out}","decimals":6}}"#),
    );
    assert!(
        (s["slope"].as_f64().unwrap() - 3.0).abs() < 1e-6,
        "slope 3: {s}"
    );
    assert!(
        (s["intercept"].as_f64().unwrap() - 2.0).abs() < 1e-6,
        "intercept 2: {s}"
    );
    assert!((s["r2"].as_f64().unwrap() - 1.0).abs() < 1e-9, "r2 1: {s}");
    assert_eq!(s["n"].as_u64().unwrap(), 4, "n=4: {s}");

    // output sheet gains predicted + residual columns; residuals ~0 here
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][2], "predicted", "predicted header: {rd}");
    assert_eq!(rows[0][3], "residual", "residual header: {rd}");
    assert!(
        (rows[1][2].as_f64().unwrap() - 5.0).abs() < 1e-6,
        "pred(1)=5: {rd}"
    );
    assert!(
        rows[1][3].as_f64().unwrap().abs() < 1e-6,
        "residual ~0: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_forecast_linear_trend() {
    let path = tmp("forecast.xlsx");
    // y = 2*index + 1 -> 1,3,5,7,9; forecast index 5,6 -> 11,13
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["y"],[1],[3],[5],[7],[9]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let out = tmp("forecast_out.xlsx");
    let r = call(
        office__sheet_forecast,
        &format!(r#"{{"path":"{path}","column":"y","periods":2,"output":"{out}","decimals":6}}"#),
    );
    assert!(
        (r["slope"].as_f64().unwrap() - 2.0).abs() < 1e-9,
        "slope 2: {r}"
    );
    assert!(
        (r["intercept"].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "intercept 1: {r}"
    );
    let fc = r["forecast"].as_array().unwrap();
    assert_eq!(fc.len(), 2, "two forecast points: {r}");
    assert!(
        (fc[0].as_f64().unwrap() - 11.0).abs() < 1e-9,
        "forecast[5]=11: {r}"
    );
    assert!(
        (fc[1].as_f64().unwrap() - 13.0).abs() < 1e-9,
        "forecast[6]=13: {r}"
    );

    // output sheet: 5 actual + 2 forecast + header = 8 rows
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 8, "header + 5 actual + 2 forecast: {rd}");
    assert_eq!(rows[7][2], "forecast", "last row is forecast: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_ttest_welch() {
    let path = tmp("ttest.xlsx");
    // a centered at 3, b centered at 12 (well separated); equal spread
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"T","rows":[
                ["a","b"],
                [1,10],
                [2,11],
                [3,12],
                [4,13],
                [5,14]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let s = call(
        office__sheet_ttest,
        &format!(r#"{{"path":"{path}","a":"a","b":"b"}}"#),
    );
    assert_eq!(s["n_a"].as_u64().unwrap(), 5, "n_a: {s}");
    assert_eq!(s["mean_a"].as_f64().unwrap(), 3.0, "mean_a: {s}");
    assert_eq!(s["mean_b"].as_f64().unwrap(), 12.0, "mean_b: {s}");
    // means 3 vs 12, SE = 1 -> t = -9
    assert!((s["t"].as_f64().unwrap() + 9.0).abs() < 1e-6, "t=-9: {s}");
    // strongly significant
    assert!(s["p"].as_f64().unwrap() < 0.001, "p tiny: {s}");

    // identical columns -> t=0, p=1
    let path2 = tmp("ttest_eq.xlsx");
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path2}","sheets":[{{"name":"T","rows":[["a","b"],[1,1],[2,2],[3,3],[4,4]]}}]}}"#
        ),
    );
    let s2 = call(
        office__sheet_ttest,
        &format!(r#"{{"path":"{path2}","a":"a","b":"b"}}"#),
    );
    assert!(
        s2["t"].as_f64().unwrap().abs() < 1e-12,
        "t=0 identical: {s2}"
    );
    assert!(
        (s2["p"].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "p=1 identical: {s2}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&path2).ok();
}

#[test]
fn sheet_anova_one_way() {
    // three groups, clearly separated means -> large F, tiny p
    let path = tmp("anova.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"A","rows":[
                ["g1","g2","g3"],
                [1,11,21],
                [2,12,22],
                [3,13,23],
                [4,14,24]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let s = call(office__sheet_anova, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(s["groups"].as_u64().unwrap(), 3, "three groups: {s}");
    assert_eq!(s["n"].as_u64().unwrap(), 12, "twelve obs: {s}");
    assert_eq!(s["df_between"].as_f64().unwrap(), 2.0, "df1=k-1: {s}");
    assert_eq!(s["df_within"].as_f64().unwrap(), 9.0, "df2=N-k: {s}");
    assert!(
        s["f"].as_f64().unwrap() > 50.0,
        "large F for separated groups: {s}"
    );
    assert!(s["p"].as_f64().unwrap() < 0.001, "tiny p: {s}");

    // identical groups -> F ~ 0, p ~ 1
    let path2 = tmp("anova_eq.xlsx");
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path2}","sheets":[{{"name":"A","rows":[["a","b"],[1,1],[2,2],[3,3]]}}]}}"#
        ),
    );
    let s2 = call(office__sheet_anova, &format!(r#"{{"path":"{path2}"}}"#));
    assert!(
        s2["f"].as_f64().unwrap() < 1e-9,
        "F~0 identical groups: {s2}"
    );
    assert!(
        (s2["p"].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "p~1 identical: {s2}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&path2).ok();
}

#[test]
fn sheet_chisq_independence() {
    // 2x2 table [[10,20],[30,40]] -> chi2 ~ 0.7937, df 1, p ~ 0.37
    let path = tmp("chisq.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"X","rows":[
                ["c1","c2"],
                [10,20],
                [30,40]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(office__sheet_chisq, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(s["rows"].as_u64().unwrap(), 2, "2 rows: {s}");
    assert_eq!(s["cols"].as_u64().unwrap(), 2, "2 cols: {s}");
    assert_eq!(s["df"].as_f64().unwrap(), 1.0, "df=(r-1)(c-1): {s}");
    assert!(
        (s["chi2"].as_f64().unwrap() - 0.79365).abs() < 1e-3,
        "chi2 ~ 0.794: {s}"
    );
    assert!(
        (0.30..0.45).contains(&s["p"].as_f64().unwrap()),
        "p ~ 0.37: {s}"
    );

    // strong association -> tiny p
    let path2 = tmp("chisq2.xlsx");
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path2}","sheets":[{{"name":"X","rows":[["a","b"],[50,5],[5,50]]}}]}}"#
        ),
    );
    let s2 = call(office__sheet_chisq, &format!(r#"{{"path":"{path2}"}}"#));
    assert!(
        s2["p"].as_f64().unwrap() < 0.001,
        "strong association tiny p: {s2}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&path2).ok();
}

#[test]
fn sheet_cov_matrix() {
    let path = tmp("cov.xlsx");
    // x = [1,2,3], y = 2x. Sample var(x) = 1; cov(x,y) = 2; var(y) = 4.
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"C","rows":[
                ["x","y"],
                [1,2],
                [2,4],
                [3,6]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(office__sheet_cov, &format!(r#"{{"path":"{path}"}}"#));
    let names = s["columns"].as_array().unwrap();
    assert_eq!(names.len(), 2, "two numeric columns: {s}");
    let m = &s["matrix"];
    assert!(
        (m[0][0].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "var(x)=1: {s}"
    );
    assert!(
        (m[0][1].as_f64().unwrap() - 2.0).abs() < 1e-9,
        "cov(x,y)=2: {s}"
    );
    assert!(
        (m[1][1].as_f64().unwrap() - 4.0).abs() < 1e-9,
        "var(y)=4: {s}"
    );
    // symmetric
    assert_eq!(m[1][0], m[0][1], "symmetric: {s}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_to_md_github_table() {
    let path = tmp("tomd.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[
                ["Item","Count"],
                ["apples",5],
                ["a|b","x"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(office__sheet_to_md, &format!(r#"{{"path":"{path}"}}"#));
    let md = r["markdown"].as_str().unwrap();
    let lines: Vec<&str> = md.lines().collect();
    assert_eq!(lines[0], "| Item | Count |", "header row: {md}");
    assert_eq!(lines[1], "| --- | --- |", "separator: {md}");
    assert_eq!(lines[2], "| apples | 5 |", "integer cell, no .0: {md}");
    assert_eq!(lines[3], r"| a\|b | x |", "pipe escaped: {md}");
    assert_eq!(r["rows"], 3, "row count: {r}");
    assert_eq!(r["cols"], 2, "col count: {r}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_to_sql_inserts() {
    let path = tmp("tosql.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[
                ["name","qty"],
                ["o'brien",5],
                ["",2]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_to_sql,
        &format!(r#"{{"path":"{path}","table":"people"}}"#),
    );
    assert_eq!(r["statements"], 2, "one statement per row: {r}");
    let sql = r["sql"].as_str().unwrap();
    assert!(
        sql.contains(r#"INSERT INTO "people" ("name", "qty") VALUES"#),
        "header columns + table: {sql}"
    );
    // numeric bare, blank -> NULL, quote doubled
    assert!(
        sql.contains("('o''brien', 5);"),
        "string escaped, number bare: {sql}"
    );
    assert!(sql.contains("(NULL, 2);"), "blank -> NULL: {sql}");

    // batch mode: both rows in one multi-row statement
    let rb = call(
        office__sheet_to_sql,
        &format!(r#"{{"path":"{path}","table":"people","batch":10}}"#),
    );
    assert_eq!(rb["statements"], 1, "batched into one statement: {rb}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_to_latex_tabular() {
    let path = tmp("tolatex.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[
                ["Item","Cost"],
                ["a&b",5],
                ["100%",10]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_to_latex,
        &format!(r#"{{"path":"{path}","booktabs":true,"caption":"My Table"}}"#),
    );
    let tex = r["latex"].as_str().unwrap();
    assert!(
        tex.contains("\\begin{tabular}{ll}"),
        "tabular + align: {tex}"
    );
    assert!(tex.contains("\\toprule"), "booktabs rule: {tex}");
    assert!(tex.contains("Item & Cost \\\\"), "header row: {tex}");
    assert!(
        tex.contains("a\\&b & 5 \\\\"),
        "ampersand escaped, int bare: {tex}"
    );
    assert!(tex.contains("100\\% & 10 \\\\"), "percent escaped: {tex}");
    assert!(tex.contains("\\caption{My Table}"), "caption float: {tex}");

    // plain (no booktabs) uses \hline
    let r2 = call(office__sheet_to_latex, &format!(r#"{{"path":"{path}"}}"#));
    assert!(
        r2["latex"].as_str().unwrap().contains("\\hline"),
        "hline rules: {r2}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_to_csv_rfc4180() {
    let path = tmp("tocsv.xlsx");
    // a field with a comma and one with a quote must be quoted/escaped
    let w = call(
        office__sheet_write,
        &serde_json::json!({
            "path": path,
            "sheets": [{ "name": "S", "rows": [["name","note"], ["a,b", 5], ["say \"hi\"", 6]] }]
        })
        .to_string(),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(office__sheet_to_csv, &format!(r#"{{"path":"{path}"}}"#));
    let csv = r["csv"].as_str().unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], "name,note", "header: {csv:?}");
    assert_eq!(
        lines[1], "\"a,b\",5",
        "comma field quoted, int bare: {csv:?}"
    );
    assert_eq!(lines[2], "\"say \"\"hi\"\"\",6", "quotes doubled: {csv:?}");

    // custom delimiter
    let rt = call(
        office__sheet_to_csv,
        &format!(r#"{{"path":"{path}","delimiter":"\t"}}"#),
    );
    assert!(
        rt["csv"].as_str().unwrap().contains("name\tnote"),
        "tab delim: {rt}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn md_to_sheet_parses_table() {
    // Leading prose then a GFM table with an escaped pipe; prose must be ignored.
    let md =
        "intro text\n\n| Item | Count |\n| :--- | ---: |\n| apples | 5 |\n| a\\|b | x |\n\nafter";
    let out = tmp("mdsheet.xlsx");
    let r = call(
        office__md_to_sheet,
        &serde_json::json!({ "markdown": md, "output": out, "name": "T" }).to_string(),
    );
    assert_eq!(r["ok"], true, "parse: {r}");
    // 2 header+body data rows kept (separator dropped): header + 2 body = 3
    assert_eq!(r["rows"], 3, "separator dropped: {r}");
    assert_eq!(r["cols"], 2, "two cols: {r}");

    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let sheet = &rd["sheets"][0];
    assert_eq!(sheet["name"], "T", "sheet name: {rd}");
    let rows = sheet["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "Item", "header cell: {rd}");
    assert_eq!(rows[1][0], "apples", "body cell: {rd}");
    assert_eq!(rows[2][0], "a|b", "pipe unescaped: {rd}");

    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_to_html_table_with_escaping() {
    let path = tmp("tohtml.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[
                ["Item","Count"],
                ["apples",5],
                ["a<b","x&y"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_to_html,
        &format!(r#"{{"path":"{path}","title":"Report"}}"#),
    );
    let html = r["html"].as_str().unwrap();
    assert!(html.contains("<h2>Report</h2>"), "title heading: {html}");
    assert!(html.contains("<thead>"), "thead present: {html}");
    assert!(html.contains("<th>Item</th>"), "header cell: {html}");
    assert!(html.contains("<td>apples</td>"), "body cell: {html}");
    assert!(html.contains("<td>5</td>"), "integer cell no .0: {html}");
    assert!(html.contains("<td>a&lt;b</td>"), "lt escaped: {html}");
    assert!(html.contains("<td>x&amp;y</td>"), "amp escaped: {html}");
    assert_eq!(r["rows"], 3, "row count: {r}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_to_text_aligned_table() {
    let path = tmp("totext.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[
                ["name","qty"],
                ["apple",5],
                ["banana",12]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // plain aligned: every line must have identical visual width
    let r = call(office__sheet_to_text, &format!(r#"{{"path":"{path}"}}"#));
    let text = r["text"].as_str().unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("name"), "header first: {text:?}");
    assert!(lines[1].contains("---"), "header underline: {text:?}");
    assert!(lines[2].starts_with("apple"), "first data row: {text:?}");
    let w0 = lines[0].chars().count();
    assert!(
        lines.iter().all(|l| l.chars().count() == w0),
        "all rows same width: {text:?}"
    );
    // integer cell rendered without .0
    assert!(lines[3].contains("12"), "banana qty: {text:?}");

    // border mode draws an ASCII grid
    let rb = call(
        office__sheet_to_text,
        &format!(r#"{{"path":"{path}","border":true}}"#),
    );
    let bt = rb["text"].as_str().unwrap();
    assert!(bt.contains("+--"), "grid corner/rule: {bt:?}");
    assert!(bt.contains("| name"), "bordered header cell: {bt:?}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_get_set_cell_a1() {
    let path = tmp("cell.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["x"]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // get existing A1
    let g = call(
        office__sheet_get_cell,
        &format!(r#"{{"path":"{path}","cell":"A1"}}"#),
    );
    assert_eq!(g["value"], "x", "A1 value: {g}");
    assert_eq!(g["row"], 0, "A1 row 0: {g}");
    assert_eq!(g["col"], 0, "A1 col 0: {g}");

    // set B2 (grows the 1x1 grid to 2x2), in place
    let s = call(
        office__sheet_set_cell,
        &format!(r#"{{"path":"{path}","cell":"B2","value":42}}"#),
    );
    assert_eq!(s["ok"], true, "set: {s}");

    // read back: B2 == 42, A1 still "x", grid grew
    let g2 = call(
        office__sheet_get_cell,
        &format!(r#"{{"path":"{path}","cell":"B2"}}"#),
    );
    assert_eq!(g2["value"].as_f64().unwrap(), 42.0, "B2 set: {g2}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "grew to 2 rows: {rd}");
    assert_eq!(rows[0][0], "x", "A1 preserved: {rd}");

    // overwrite A1 with a string
    call(
        office__sheet_set_cell,
        &format!(r#"{{"path":"{path}","cell":"A1","value":"hi"}}"#),
    );
    let g3 = call(
        office__sheet_get_cell,
        &format!(r#"{{"path":"{path}","cell":"A1"}}"#),
    );
    assert_eq!(g3["value"], "hi", "A1 overwritten: {g3}");

    // AA1 parses as column 27 (0-based 26)
    let g4 = call(
        office__sheet_get_cell,
        &format!(r#"{{"path":"{path}","cell":"AA1"}}"#),
    );
    assert_eq!(g4["col"], 26, "AA -> col 26: {g4}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_get_set_range_a1() {
    let path = tmp("range.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[[1,2,3],[4,5,6],[7,8,9]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // read the A1:B2 sub-block
    let g = call(
        office__sheet_get_range,
        &format!(r#"{{"path":"{path}","range":"A1:B2"}}"#),
    );
    assert_eq!(g["nrows"], 2, "2 rows: {g}");
    assert_eq!(g["ncols"], 2, "2 cols: {g}");
    assert_eq!(g["rows"][0][0].as_f64().unwrap(), 1.0, "A1: {g}");
    assert_eq!(g["rows"][1][1].as_f64().unwrap(), 5.0, "B2: {g}");

    // paste a 2x2 block at B2; original A-column + row1 untouched
    let s = call(
        office__sheet_set_range,
        &serde_json::json!({
            "path": path, "cell": "B2", "values": [[20, 30], [50, 60]]
        })
        .to_string(),
    );
    assert_eq!(s["cells"], 4, "4 cells written: {s}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][1].as_f64().unwrap(), 20.0, "B2 pasted: {rd}");
    assert_eq!(rows[2][2].as_f64().unwrap(), 60.0, "C3 pasted: {rd}");
    assert_eq!(rows[0][0].as_f64().unwrap(), 1.0, "A1 untouched: {rd}");
    assert_eq!(rows[1][0].as_f64().unwrap(), 4.0, "A2 untouched: {rd}");

    // set_range grows the grid beyond current bounds
    let s2 = call(
        office__sheet_set_range,
        &serde_json::json!({ "path": path, "cell": "E5", "values": [["x"]] }).to_string(),
    );
    assert_eq!(s2["ok"], true, "grow set: {s2}");
    let g2 = call(
        office__sheet_get_range,
        &format!(r#"{{"path":"{path}","range":"E5"}}"#),
    );
    assert_eq!(g2["rows"][0][0], "x", "E5 grown + set: {g2}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_insert_delete_rows() {
    let path = tmp("rowops.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["h"],[1],[2],[3]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // delete rows 2..3 (the "1" and "2" data rows) -> header + "3"
    let d = call(
        office__sheet_delete_rows,
        &format!(r#"{{"path":"{path}","at":2,"count":2}}"#),
    );
    assert_eq!(d["deleted"], 2, "two rows deleted: {d}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "header + 1 row: {rd}");
    assert_eq!(rows[0][0], "h", "header kept: {rd}");
    assert_eq!(
        rows[1][0].as_f64().unwrap(),
        3.0,
        "remaining row is 3: {rd}"
    );

    // insert a blank row at position 2, fill it, confirm the shift
    let i = call(
        office__sheet_insert_rows,
        &format!(r#"{{"path":"{path}","at":2,"count":1}}"#),
    );
    assert_eq!(i["inserted"], 1, "one row inserted: {i}");
    call(
        office__sheet_set_cell,
        &format!(r#"{{"path":"{path}","cell":"A2","value":"new"}}"#),
    );
    let rd2 = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    let rows2 = rd2["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows2[1][0], "new", "inserted row filled at A2: {rd2}");
    assert_eq!(
        rows2[2][0].as_f64().unwrap(),
        3.0,
        "old row shifted down: {rd2}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_insert_column_shifts_right() {
    let path = tmp("colins.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["a","b"],[1,2],[3,4]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // insert a "mid" column at position 2, fill data rows with 0
    let r = call(
        office__sheet_insert_column,
        &format!(r#"{{"path":"{path}","at":2,"name":"mid","value":0}}"#),
    );
    assert_eq!(r["at"], 2, "inserted at col 2: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "a", "col a stays: {rd}");
    assert_eq!(rows[0][1], "mid", "new header inserted: {rd}");
    assert_eq!(rows[0][2], "b", "col b shifted right: {rd}");
    assert_eq!(rows[1][1].as_f64().unwrap(), 0.0, "data fill: {rd}");
    assert_eq!(rows[1][2].as_f64().unwrap(), 2.0, "b value shifted: {rd}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_group_pct_share_within_group() {
    let path = tmp("grouppct.xlsx");
    // west: 10+30=40 → 25%/75%; east: 5 alone → 100%
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","amt"],
                ["west",10],
                ["east",5],
                ["west",30]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("grouppct_out.xlsx");
    let r = call(
        office__sheet_group_pct,
        &format!(r#"{{"path":"{path}","output":"{out}","group":"region","value":"amt"}}"#),
    );
    assert_eq!(r["column"], "amt_grouppct", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert!(
        (rows[1][2].as_f64().unwrap() - 25.0).abs() < 1e-9,
        "west 10/40=25%: {rd}"
    );
    assert!(
        (rows[2][2].as_f64().unwrap() - 100.0).abs() < 1e-9,
        "east 5/5=100%: {rd}"
    );
    assert!(
        (rows[3][2].as_f64().unwrap() - 75.0).abs() < 1e-9,
        "west 30/40=75%: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_cumcount_occurrences() {
    let path = tmp("cumcount.xlsx");
    // a,a,b,a -> counts 0,1,0,2
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["g"],["a"],["a"],["b"],["a"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let out = tmp("cumcount_out.xlsx");
    let r = call(
        office__sheet_cumcount,
        &format!(r#"{{"path":"{path}","column":"g","output":"{out}","into":"n"}}"#),
    );
    assert_eq!(r["column"], "n", "new column: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][1].as_f64().unwrap(), 0.0, "first a -> 0: {rd}");
    assert_eq!(rows[2][1].as_f64().unwrap(), 1.0, "second a -> 1: {rd}");
    assert_eq!(rows[3][1].as_f64().unwrap(), 0.0, "first b -> 0: {rd}");
    assert_eq!(rows[4][1].as_f64().unwrap(), 2.0, "third a -> 2: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_running_per_group() {
    let path = tmp("running.xlsx");
    // west: 10, then +30 = 40 ; east: 5 (independent running total)
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","amt"],
                ["west",10],
                ["east",5],
                ["west",30]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("running_out.xlsx");
    let r = call(
        office__sheet_running,
        &format!(r#"{{"path":"{path}","output":"{out}","group":"region","value":"amt"}}"#),
    );
    assert_eq!(r["column"], "amt_running", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][2].as_f64().unwrap(), 10.0, "west first = 10: {rd}");
    assert_eq!(
        rows[2][2].as_f64().unwrap(),
        5.0,
        "east independent = 5: {rd}"
    );
    assert_eq!(
        rows[3][2].as_f64().unwrap(),
        40.0,
        "west accumulates to 40: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_cumsum_running_total() {
    let path = tmp("cumsum.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["amt"],[10],[20],[30]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_cumsum,
        &format!(r#"{{"path":"{path}","column":"amt","output":"{path}"}}"#),
    );
    assert_eq!(r["column"], "amt_cumsum", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][1], "amt_cumsum", "header appended: {rd}");
    assert_eq!(rows[1][1].as_f64().unwrap(), 10.0, "cumsum 1: {rd}");
    assert_eq!(rows[2][1].as_f64().unwrap(), 30.0, "cumsum 2: {rd}");
    assert_eq!(rows[3][1].as_f64().unwrap(), 60.0, "cumsum 3: {rd}");
    // original column untouched
    assert_eq!(rows[2][0].as_f64().unwrap(), 20.0, "source kept: {rd}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_cumulative_max_and_prod() {
    let path = tmp("cumul.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[3],[1],[4],[2]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // running max → 3, 3, 4, 4
    let out = tmp("cumul_max.xlsx");
    let r = call(
        office__sheet_cumulative,
        &format!(r#"{{"path":"{path}","column":"v","output":"{out}"}}"#),
    );
    assert_eq!(r["column"], "v_cummax", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][1].as_f64().unwrap(), 3.0, "cummax 1: {rd}");
    assert_eq!(rows[2][1].as_f64().unwrap(), 3.0, "cummax 2 (1<3): {rd}");
    assert_eq!(rows[3][1].as_f64().unwrap(), 4.0, "cummax 3: {rd}");
    assert_eq!(rows[4][1].as_f64().unwrap(), 4.0, "cummax 4 (2<4): {rd}");

    // running product → 3, 3, 12, 24
    let outp = tmp("cumul_prod.xlsx");
    let rp = call(
        office__sheet_cumulative,
        &format!(r#"{{"path":"{path}","column":"v","output":"{outp}","agg":"prod"}}"#),
    );
    assert_eq!(rp["column"], "v_cumprod", "prod column name: {rp}");
    let rdp = call(office__sheet_read, &format!(r#"{{"path":"{outp}"}}"#));
    let rowsp = rdp["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(
        rowsp[3][1].as_f64().unwrap(),
        12.0,
        "cumprod 3*1*4=12: {rdp}"
    );
    assert_eq!(rowsp[4][1].as_f64().unwrap(), 24.0, "cumprod *2=24: {rdp}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outp).ok();
}

#[test]
fn sheet_pct_of_total() {
    let path = tmp("pct.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["amt"],[100],[300]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_pct,
        &format!(r#"{{"path":"{path}","column":"amt","output":"{path}","decimals":1}}"#),
    );
    assert_eq!(r["column"], "amt_pct", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{path}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][1], "amt_pct", "header appended: {rd}");
    // 100/400 = 25%, 300/400 = 75%
    assert!(
        (rows[1][1].as_f64().unwrap() - 25.0).abs() < 1e-9,
        "pct 25: {rd}"
    );
    assert!(
        (rows[2][1].as_f64().unwrap() - 75.0).abs() < 1e-9,
        "pct 75: {rd}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_normalize_minmax_and_zscore() {
    let path = tmp("norm.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[10],[20],[30]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // min-max scaling -> 0, 0.5, 1
    let mm = tmp("norm_mm.xlsx");
    let r = call(
        office__sheet_normalize,
        &format!(r#"{{"path":"{path}","column":"v","output":"{mm}"}}"#),
    );
    assert_eq!(r["column"], "v_norm", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{mm}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][1], "v_norm", "header appended: {rd}");
    assert!(
        (rows[1][1].as_f64().unwrap() - 0.0).abs() < 1e-9,
        "min -> 0: {rd}"
    );
    assert!(
        (rows[2][1].as_f64().unwrap() - 0.5).abs() < 1e-9,
        "mid -> 0.5: {rd}"
    );
    assert!(
        (rows[3][1].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "max -> 1: {rd}"
    );

    // z-score: mean 20, population std sqrt(200/3)~8.165 -> -1.2247, 0, 1.2247
    let zs = tmp("norm_zs.xlsx");
    call(
        office__sheet_normalize,
        &format!(
            r#"{{"path":"{path}","column":"v","output":"{zs}","method":"zscore","decimals":3}}"#
        ),
    );
    let rz = call(office__sheet_read, &format!(r#"{{"path":"{zs}"}}"#));
    let rows2 = rz["sheets"][0]["rows"].as_array().unwrap();
    assert!(
        (rows2[2][1].as_f64().unwrap() - 0.0).abs() < 1e-9,
        "mean -> 0: {rz}"
    );
    assert!(
        (rows2[1][1].as_f64().unwrap() + 1.225).abs() < 0.01,
        "low z<0: {rz}"
    );
    assert!(
        (rows2[3][1].as_f64().unwrap() - 1.225).abs() < 0.01,
        "high z>0: {rz}"
    );

    for f in [&path, &mm, &zs] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_standardize_in_place() {
    let path = tmp("std.xlsx");
    // v: 10,20,30 (mean 20, pop std ~8.165); label column left alone
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v","name"],[10,"a"],[20,"b"],[30,"c"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("std_out.xlsx");
    let r = call(
        office__sheet_standardize,
        &format!(r#"{{"path":"{path}","output":"{out}","decimals":3}}"#),
    );
    assert_eq!(r["columns"], 1, "only the numeric column standardized: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert!(
        (rows[2][0].as_f64().unwrap()).abs() < 1e-9,
        "mean -> 0 in place: {rd}"
    );
    assert!(
        (rows[1][0].as_f64().unwrap() + 1.225).abs() < 0.01,
        "low z<0: {rd}"
    );
    assert!(
        (rows[3][0].as_f64().unwrap() - 1.225).abs() < 0.01,
        "high z>0: {rd}"
    );
    // text column untouched
    assert_eq!(rows[1][1], "a", "label column preserved: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_movavg_rolling_mean() {
    let path = tmp("movavg.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[10],[20],[30],[40]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // window 2: row0 blank, then 15, 25, 35
    let out = tmp("movavg_out.xlsx");
    let r = call(
        office__sheet_movavg,
        &format!(r#"{{"path":"{path}","column":"v","window":2,"output":"{out}"}}"#),
    );
    assert_eq!(r["column"], "v_ma2", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][1], "v_ma2", "header appended: {rd}");
    // first data row: window not filled -> blank (empty/null round-trips)
    assert_eq!(rows[1][1].as_str().unwrap_or(""), "", "row1 blank: {rd}");
    assert!(
        (rows[2][1].as_f64().unwrap() - 15.0).abs() < 1e-9,
        "ma 15: {rd}"
    );
    assert!(
        (rows[3][1].as_f64().unwrap() - 25.0).abs() < 1e-9,
        "ma 25: {rd}"
    );
    assert!(
        (rows[4][1].as_f64().unwrap() - 35.0).abs() < 1e-9,
        "ma 35: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_ewm_exponential() {
    let path = tmp("ewm.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[10],[20],[30]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // alpha 0.5: y0=10, y1=0.5*20+0.5*10=15, y2=0.5*30+0.5*15=22.5
    let out = tmp("ewm_out.xlsx");
    let r = call(
        office__sheet_ewm,
        &format!(r#"{{"path":"{path}","column":"v","alpha":0.5,"output":"{out}"}}"#),
    );
    assert_eq!(r["column"], "v_ewm", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][1], "v_ewm", "header appended: {rd}");
    assert!(
        (rows[1][1].as_f64().unwrap() - 10.0).abs() < 1e-9,
        "y0 = x0 = 10: {rd}"
    );
    assert!(
        (rows[2][1].as_f64().unwrap() - 15.0).abs() < 1e-9,
        "y1 = 15: {rd}"
    );
    assert!(
        (rows[3][1].as_f64().unwrap() - 22.5).abs() < 1e-9,
        "y2 = 22.5: {rd}"
    );

    // span 1 -> alpha = 2/(1+1) = 1.0 -> output equals input
    let outs = tmp("ewm_span.xlsx");
    call(
        office__sheet_ewm,
        &format!(r#"{{"path":"{path}","column":"v","span":1,"output":"{outs}"}}"#),
    );
    let rds = call(office__sheet_read, &format!(r#"{{"path":"{outs}"}}"#));
    assert!(
        (rds["sheets"][0]["rows"][3][1].as_f64().unwrap() - 30.0).abs() < 1e-9,
        "span 1 (alpha 1) tracks input: {rds}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outs).ok();
}

#[test]
fn sheet_rolling_aggregates() {
    let path = tmp("rolling.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[10],[20],[30],[40]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // rolling sum, window 3 → row3 = 60, row4 = 90
    let out = tmp("rolling_sum.xlsx");
    let r = call(
        office__sheet_rolling,
        &format!(r#"{{"path":"{path}","column":"v","window":3,"agg":"sum","output":"{out}"}}"#),
    );
    assert_eq!(r["column"], "v_roll3", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][1].as_str().unwrap_or(""), "", "row1 blank: {rd}");
    assert_eq!(rows[2][1].as_str().unwrap_or(""), "", "row2 blank: {rd}");
    assert!(
        (rows[3][1].as_f64().unwrap() - 60.0).abs() < 1e-9,
        "sum 60: {rd}"
    );
    assert!(
        (rows[4][1].as_f64().unwrap() - 90.0).abs() < 1e-9,
        "sum 90: {rd}"
    );

    // rolling max, window 2 → 20, 30, 40
    let outm = tmp("rolling_max.xlsx");
    let rm = call(
        office__sheet_rolling,
        &format!(r#"{{"path":"{path}","column":"v","window":2,"agg":"max","output":"{outm}"}}"#),
    );
    assert_eq!(rm["ok"], true, "rolling max: {rm}");
    let rdm = call(office__sheet_read, &format!(r#"{{"path":"{outm}"}}"#));
    let rowsm = rdm["sheets"][0]["rows"].as_array().unwrap();
    assert!(
        (rowsm[2][1].as_f64().unwrap() - 20.0).abs() < 1e-9,
        "max 20: {rdm}"
    );
    assert!(
        (rowsm[4][1].as_f64().unwrap() - 40.0).abs() < 1e-9,
        "max 40: {rdm}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outm).ok();
}

#[test]
fn sheet_bollinger_bands() {
    let path = tmp("boll.xlsx");
    // window 3: rows 3 and 4 fill; mid = rolling mean
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[2],[4],[6],[8]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let out = tmp("boll_out.xlsx");
    let r = call(
        office__sheet_bollinger,
        &format!(
            r#"{{"path":"{path}","column":"v","window":3,"k":1,"output":"{out}","decimals":4}}"#
        ),
    );
    let names = r["columns"].as_array().unwrap();
    assert_eq!(names[0], "bb_mid", "mid column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][1], "bb_mid", "header appended: {rd}");
    assert!(
        rows[2][1].as_f64().is_none(),
        "row before window fills is blank: {rd}"
    );
    assert_eq!(rows[3][1].as_f64().unwrap(), 4.0, "mid of 2,4,6 = 4: {rd}");
    assert_eq!(rows[4][1].as_f64().unwrap(), 6.0, "mid of 4,6,8 = 6: {rd}");
    // upper band above mid, lower below
    assert!(rows[3][2].as_f64().unwrap() > 4.0, "upper > mid: {rd}");
    assert!(rows[3][3].as_f64().unwrap() < 4.0, "lower < mid: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_delta_row_over_row() {
    let path = tmp("delta.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[10],[30],[25]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("delta_out.xlsx");
    let r = call(
        office__sheet_delta,
        &format!(r#"{{"path":"{path}","column":"v","output":"{out}"}}"#),
    );
    assert_eq!(r["column"], "v_delta", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][1], "v_delta", "header appended: {rd}");
    assert_eq!(
        rows[1][1].as_str().unwrap_or(""),
        "",
        "first row blank: {rd}"
    );
    assert!(
        (rows[2][1].as_f64().unwrap() - 20.0).abs() < 1e-9,
        "30-10=20: {rd}"
    );
    assert!(
        (rows[3][1].as_f64().unwrap() + 5.0).abs() < 1e-9,
        "25-30=-5: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_pct_change_row_over_row() {
    let path = tmp("pctchg.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[100],[150],[75]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // default: percent. 150 from 100 = +50%; 75 from 150 = -50%.
    let out = tmp("pctchg_out.xlsx");
    let r = call(
        office__sheet_pct_change,
        &format!(r#"{{"path":"{path}","column":"v","output":"{out}"}}"#),
    );
    assert_eq!(r["column"], "v_pctchg", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(
        rows[1][1].as_str().unwrap_or(""),
        "",
        "first row blank: {rd}"
    );
    assert!(
        (rows[2][1].as_f64().unwrap() - 50.0).abs() < 1e-9,
        "+50%: {rd}"
    );
    assert!(
        (rows[3][1].as_f64().unwrap() + 50.0).abs() < 1e-9,
        "-50%: {rd}"
    );

    // fraction mode: raw ratio (0.5, -0.5)
    let outf = tmp("pctchg_frac.xlsx");
    let rf = call(
        office__sheet_pct_change,
        &format!(r#"{{"path":"{path}","column":"v","output":"{outf}","fraction":true}}"#),
    );
    assert_eq!(rf["ok"], true, "fraction mode: {rf}");
    let rdf = call(office__sheet_read, &format!(r#"{{"path":"{outf}"}}"#));
    let rowsf = rdf["sheets"][0]["rows"].as_array().unwrap();
    assert!(
        (rowsf[2][1].as_f64().unwrap() - 0.5).abs() < 1e-9,
        "+0.5 ratio: {rdf}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outf).ok();
}

#[test]
fn sheet_shift_lag_and_lead() {
    let path = tmp("shift.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[10],[20],[30]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // lag 1 (default): [blank, 10, 20]
    let out = tmp("shift_lag.xlsx");
    let r = call(
        office__sheet_shift,
        &format!(r#"{{"path":"{path}","column":"v","output":"{out}"}}"#),
    );
    assert_eq!(r["column"], "v_shift1", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(
        rows[1][1].as_str().unwrap_or(""),
        "",
        "first row vacated: {rd}"
    );
    assert_eq!(rows[2][1].as_f64().unwrap(), 10.0, "row2 = prev 10: {rd}");
    assert_eq!(rows[3][1].as_f64().unwrap(), 20.0, "row3 = prev 20: {rd}");

    // lead 1 (periods = -1): [20, 30, blank], with explicit fill
    let outl = tmp("shift_lead.xlsx");
    let rl = call(
        office__sheet_shift,
        &format!(r#"{{"path":"{path}","column":"v","output":"{outl}","periods":-1,"fill":"NA"}}"#),
    );
    assert_eq!(rl["ok"], true, "lead shift: {rl}");
    let rdl = call(office__sheet_read, &format!(r#"{{"path":"{outl}"}}"#));
    let rowsl = rdl["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rowsl[1][1].as_f64().unwrap(), 20.0, "row1 = next 20: {rdl}");
    assert_eq!(rowsl[3][1], "NA", "last row filled with NA: {rdl}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outl).ok();
}

#[test]
fn sheet_clamp_caps_values() {
    let path = tmp("clamp.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[5],[15],[25],["x"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // clamp to [10, 20] in place
    let out = tmp("clamp_out.xlsx");
    let r = call(
        office__sheet_clamp,
        &format!(r#"{{"path":"{path}","column":"v","min":10,"max":20,"output":"{out}"}}"#),
    );
    assert_eq!(r["clamped"], 2, "two values capped (5 and 25): {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    // single-column sheet: clamp-in-place rewrites column 0
    assert!(
        (rows[1][0].as_f64().unwrap() - 10.0).abs() < 1e-9,
        "5 -> 10: {rd}"
    );
    assert!(
        (rows[2][0].as_f64().unwrap() - 15.0).abs() < 1e-9,
        "15 unchanged: {rd}"
    );
    assert!(
        (rows[3][0].as_f64().unwrap() - 20.0).abs() < 1e-9,
        "25 -> 20: {rd}"
    );
    assert_eq!(rows[4][0], "x", "non-numeric passes through: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_winsorize_percentile_clip() {
    let path = tmp("winsor.xlsx");
    // 1..10 with default 5%/95% bounds → low≈1.45, high≈9.55: 1→1.45, 10→9.55
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[1],[2],[3],[4],[5],[6],[7],[8],[9],[10]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("winsor_out.xlsx");
    let r = call(
        office__sheet_winsorize,
        &format!(r#"{{"path":"{path}","column":"v","output":"{out}","decimals":2}}"#),
    );
    // low = p05, high = p95 of 1..10 (linear interp): 1.45 and 9.55
    assert!(
        (r["low"].as_f64().unwrap() - 1.45).abs() < 0.01,
        "low bound: {r}"
    );
    assert!(
        (r["high"].as_f64().unwrap() - 9.55).abs() < 0.01,
        "high bound: {r}"
    );
    assert_eq!(r["clipped"], 2, "the two extremes clipped: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert!(
        (rows[1][0].as_f64().unwrap() - 1.45).abs() < 0.01,
        "1 -> low: {rd}"
    );
    assert!(
        (rows[10][0].as_f64().unwrap() - 9.55).abs() < 0.01,
        "10 -> high: {rd}"
    );
    assert!(
        (rows[5][0].as_f64().unwrap() - 5.0).abs() < 1e-9,
        "mid unchanged: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_rename_column_header() {
    let path = tmp("rencol.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["a","b"],[1,2]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("rencol_out.xlsx");
    let r = call(
        office__sheet_rename_column,
        &format!(r#"{{"path":"{path}","column":"a","to":"id","output":"{out}"}}"#),
    );
    assert_eq!(r["column"], "id", "new header name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "id", "column a renamed: {rd}");
    assert_eq!(rows[0][1], "b", "other header intact: {rd}");
    // data untouched
    assert_eq!(rows[1][0].as_f64().unwrap(), 1.0, "data kept: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_rename_columns_bulk() {
    let path = tmp("rencols.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["a","b","c"],[1,2,3]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("rencols_out.xlsx");
    let r = call(
        office__sheet_rename_columns,
        &serde_json::json!({
            "path": path, "output": out,
            "map": { "a": "id", "c": "total", "missing": "x" }
        })
        .to_string(),
    );
    // only a and c exist -> 2 renamed (missing key ignored)
    assert_eq!(r["renamed"], 2, "two headers renamed: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "id", "a -> id: {rd}");
    assert_eq!(rows[0][1], "b", "b unchanged: {rd}");
    assert_eq!(rows[0][2], "total", "c -> total: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_explode_delimited_column() {
    let path = tmp("explode.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["id","tags"],[1,"a,b"],[2,"c"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("explode_out.xlsx");
    let r = call(
        office__sheet_explode,
        &format!(r#"{{"path":"{path}","column":"tags","output":"{out}","sep":","}}"#),
    );
    assert_eq!(r["rows"], 3, "1/a,1/b,2/c -> 3 rows: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4, "header + 3 data: {rd}");
    assert_eq!(
        rows[1][0].as_f64().unwrap(),
        1.0,
        "row1 id duplicated: {rd}"
    );
    assert_eq!(rows[1][1], "a", "row1 tag a: {rd}");
    assert_eq!(
        rows[2][0].as_f64().unwrap(),
        1.0,
        "row2 id duplicated: {rd}"
    );
    assert_eq!(rows[2][1], "b", "row2 tag b: {rd}");
    assert_eq!(rows[3][1], "c", "row3 tag c: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_map_recodes_values() {
    let path = tmp("smap.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["sex"],["M"],["F"],["M"],["?"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // recode M/F, leave "?" unchanged (no default)
    let out = tmp("smap_out.xlsx");
    let r = call(
        office__sheet_map,
        &serde_json::json!({
            "path": path, "column": "sex",
            "mapping": {"M": "Male", "F": "Female"}, "output": out
        })
        .to_string(),
    );
    assert_eq!(r["mapped"], 3, "three cells recoded: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][0], "Male", "M -> Male: {rd}");
    assert_eq!(rows[2][0], "Female", "F -> Female: {rd}");
    assert_eq!(rows[4][0], "?", "unmapped kept: {rd}");

    // with default: unmapped -> "Other"
    let outd = tmp("smap_d.xlsx");
    call(
        office__sheet_map,
        &serde_json::json!({
            "path": path, "column": "sex",
            "mapping": {"M": "Male"}, "default": "Other", "output": outd
        })
        .to_string(),
    );
    let rdd = call(office__sheet_read, &format!(r#"{{"path":"{outd}"}}"#));
    assert_eq!(
        rdd["sheets"][0]["rows"][2][0], "Other",
        "F -> default Other: {rdd}"
    );

    for f in [&path, &out, &outd] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_partition_by_column() {
    let path = tmp("partition.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","amt"],
                ["west",10],
                ["east",5],
                ["west",20]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let dir = tmp("partition_out");
    std::fs::create_dir_all(&dir).unwrap();
    let r = call(
        office__sheet_partition,
        &format!(r#"{{"path":"{path}","column":"region","dir":"{dir}","prefix":"r-"}}"#),
    );
    assert_eq!(r["count"], 2, "two partitions: {r}");

    // the west file has the header + its 2 rows
    let west = call(
        office__sheet_read,
        &format!(r#"{{"path":"{dir}/r-west.xlsx"}}"#),
    );
    let rows = west["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "header + 2 west rows: {west}");
    assert_eq!(rows[0][0], "region", "header repeated: {west}");
    assert_eq!(rows[1][0], "west", "west row: {west}");
    assert_eq!(
        rows[2][1].as_f64().unwrap(),
        20.0,
        "second west amt: {west}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sheet_split_by_into_tabs() {
    let path = tmp("splitby.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","amt"],
                ["west",10],
                ["east",5],
                ["west",20]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("splitby_out.xlsx");
    let r = call(
        office__sheet_split_by,
        &format!(r#"{{"path":"{path}","output":"{out}","column":"region"}}"#),
    );
    assert_eq!(r["sheets"], 2, "two tabs: {r}");
    let groups: Vec<String> = r["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(groups, vec!["west", "east"], "first-seen tab order: {r}");

    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let sheets = rd["sheets"].as_array().unwrap();
    assert_eq!(sheets[0]["name"], "west", "first tab named west: {rd}");
    // west tab: header + 2 west rows
    let wrows = sheets[0]["rows"].as_array().unwrap();
    assert_eq!(wrows.len(), 3, "header + 2 west rows: {rd}");
    assert_eq!(wrows[2][1].as_f64().unwrap(), 20.0, "second west amt: {rd}");
    // east tab: header + 1 row
    assert_eq!(
        sheets[1]["rows"].as_array().unwrap().len(),
        2,
        "header + 1 east: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_multisort_two_keys() {
    let path = tmp("msort.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","amt"],
                ["west",10],
                ["east",20],
                ["west",5]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // region ascending, then amt descending
    let out = tmp("msort_out.xlsx");
    let r = call(
        office__sheet_multisort,
        &serde_json::json!({
            "path": path,
            "keys": [{"column": "region"}, {"column": "amt", "descending": true}],
            "output": out
        })
        .to_string(),
    );
    assert_eq!(r["sorted"], 3, "three rows sorted: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][0], "east", "east first (region asc): {rd}");
    assert_eq!(rows[2][0], "west", "then west: {rd}");
    assert_eq!(
        rows[2][1].as_f64().unwrap(),
        10.0,
        "west 10 before 5 (amt desc): {rd}"
    );
    assert_eq!(rows[3][1].as_f64().unwrap(), 5.0, "west 5 last: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_find_locates_cells() {
    let path = tmp("find.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"People","rows":[
                ["name","city"],
                ["alice","NYC"],
                ["bob","LA"],
                ["Alice","SF"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // case-insensitive substring: both alice/Alice
    let ci = call(
        office__sheet_find,
        &format!(r#"{{"path":"{path}","query":"alice","ignore_case":true}}"#),
    );
    assert_eq!(ci["count"], 2, "ci matches both: {ci}");
    let refs: Vec<&str> = ci["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["ref"].as_str().unwrap())
        .collect();
    assert!(
        refs.contains(&"A2") && refs.contains(&"A4"),
        "A1 refs: {ci}"
    );

    // case-sensitive: only "Alice"
    let cs = call(
        office__sheet_find,
        &format!(r#"{{"path":"{path}","query":"Alice"}}"#),
    );
    assert_eq!(cs["count"], 1, "cs one match: {cs}");
    assert_eq!(cs["matches"][0]["ref"], "A4", "cs ref: {cs}");

    // whole-cell match
    let wh = call(
        office__sheet_find,
        &format!(r#"{{"path":"{path}","query":"LA","whole":true}}"#),
    );
    assert_eq!(wh["count"], 1, "whole match: {wh}");
    assert_eq!(wh["matches"][0]["ref"], "B3", "whole ref: {wh}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_write_html_and_markdown_tables() {
    // HTML table
    let h = tmp("table.html");
    let wh = call(
        office__sheet_write,
        &format!(r#"{{"path":"{h}","sheets":[{{"name":"D","rows":[["a","b"],[1,2]]}}]}}"#),
    );
    assert_eq!(wh["ok"], true, "html write: {wh}");
    let html = std::fs::read_to_string(&h).unwrap();
    assert!(html.contains("<th>a</th>"), "header th: {html}");
    assert!(html.contains("<td>1</td>"), "data td: {html}");

    // Markdown table
    let m = tmp("table.md");
    let wm = call(
        office__sheet_write,
        &format!(r#"{{"path":"{m}","sheets":[{{"name":"D","rows":[["a","b"],[1,2]]}}]}}"#),
    );
    assert_eq!(wm["ok"], true, "md write: {wm}");
    let md = std::fs::read_to_string(&m).unwrap();
    assert!(md.contains("| a | b |"), "md header row: {md}");
    assert!(md.contains("| --- | --- |"), "md separator: {md}");
    assert!(md.contains("| 1 | 2 |"), "md data row: {md}");

    std::fs::remove_file(&h).ok();
    std::fs::remove_file(&m).ok();
}

#[test]
fn sheet_validate_rules() {
    let path = tmp("val.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","age"],["Alice",30],["",17],["Bob","x"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_validate,
        &format!(
            r#"{{"path":"{path}","rules":[{{"column":"name","type":"nonempty"}},{{"column":"age","type":"number","min":18}}]}}"#
        ),
    );
    assert_eq!(r["valid"], false, "invalid: {r}");
    assert_eq!(r["count"], 3, "three violations: {r}");
    let refs: Vec<&str> = r["violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["ref"].as_str().unwrap())
        .collect();
    assert!(refs.contains(&"A3"), "name blank at A3: {r}");
    assert!(refs.contains(&"B3"), "age<18 at B3: {r}");
    assert!(refs.contains(&"B4"), "age not number at B4: {r}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_to_slides_per_row() {
    let path = tmp("s2s.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","role","city"],["Alice","Eng","NYC"],["Bob","PM","LA"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("s2s.pptx");
    let r = call(
        office__sheet_to_slides,
        &format!(r#"{{"path":"{path}","output":"{out}","title_field":"name"}}"#),
    );
    assert_eq!(r["slides"], 2, "two slides: {r}");
    let rd = call(office__slides_read, &format!(r#"{{"path":"{out}"}}"#));
    let s0 = rd["slides"][0]["text"].to_string();
    assert!(s0.contains("Alice"), "title: {s0}");
    assert!(
        s0.contains("role: Eng") && s0.contains("city: NYC"),
        "body fields: {s0}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn info_identifies_file_types() {
    // spreadsheet
    let xlsx = tmp("info.xlsx");
    call(
        office__sheet_write,
        &format!(r#"{{"path":"{xlsx}","sheets":[{{"name":"A","rows":[["x"],[1]]}}]}}"#),
    );
    let si = call(office__info, &format!(r#"{{"path":"{xlsx}"}}"#));
    assert_eq!(si["type"], "spreadsheet", "xlsx type: {si}");
    assert_eq!(si["sheets"][0]["name"], "A", "sheet listed: {si}");

    // pdf
    let pdf = tmp("info.pdf");
    call(
        office__pdf_build,
        &format!(r#"{{"path":"{pdf}","elements":[{{"type":"heading","level":1,"text":"H"}}]}}"#),
    );
    let pi = call(office__info, &format!(r#"{{"path":"{pdf}"}}"#));
    assert_eq!(pi["type"], "pdf", "pdf type: {pi}");
    assert_eq!(pi["pages"], 1, "pdf pages: {pi}");

    // image
    let png = tmp("info.png");
    let n = call(
        office__img_new,
        r#"{"width":12,"height":7,"color":[0,0,0,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    call(
        office__img_save,
        &format!(r#"{{"handle":{h},"path":"{png}"}}"#),
    );
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    let ii = call(office__info, &format!(r#"{{"path":"{png}"}}"#));
    assert_eq!(ii["type"], "image", "image type: {ii}");
    assert_eq!(ii["width"], 12, "image width: {ii}");

    for f in [&xlsx, &pdf, &png] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_info_dimensions() {
    let path = tmp("wbinfo.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[
                {{"name":"A","rows":[["x"],[1],[2]]}},
                {{"name":"B","rows":[["y","z"],[1,2]]}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let r = call(office__sheet_info, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["count"], 2, "two sheets: {r}");
    assert_eq!(r["sheets"][0]["name"], "A");
    assert_eq!(r["sheets"][0]["rows"], 3, "A rows: {r}");
    assert_eq!(r["sheets"][0]["cols"], 1, "A cols: {r}");
    assert_eq!(r["sheets"][1]["name"], "B");
    assert_eq!(r["sheets"][1]["cols"], 2, "B cols: {r}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn img_caption_adds_bar() {
    // make a 40x20 source image
    let png = tmp("capsrc.png");
    let n = call(
        office__img_new,
        r#"{"width":40,"height":20,"color":[0,128,255,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    call(
        office__img_save,
        &format!(r#"{{"handle":{h},"path":"{png}"}}"#),
    );
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let out = tmp("capout.png");
    let r = call(
        office__img_caption,
        &format!(r#"{{"input":"{png}","output":"{out}","text":"Hello","height":30,"size":16}}"#),
    );
    assert_eq!(r["ok"], true, "caption: {r}");
    assert_eq!(r["width"], 40, "width unchanged: {r}");
    assert_eq!(r["height"], 50, "height = 20 + 30 bar: {r}");
    // re-open to confirm a valid taller image was written
    let re = call(office__img_open, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(re["height"], 50, "saved image is taller: {re}");
    call(
        office__img_close,
        &format!(r#"{{"handle":{}}}"#, re["handle"]),
    );

    std::fs::remove_file(&png).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_diff_cells() {
    let a = tmp("diffa.xlsx");
    let b = tmp("diffb.xlsx");
    call(
        office__sheet_write,
        &format!(r#"{{"path":"{a}","sheets":[{{"name":"D","rows":[["a","b"],[1,2],[3,4]]}}]}}"#),
    );
    call(
        office__sheet_write,
        &format!(r#"{{"path":"{b}","sheets":[{{"name":"D","rows":[["a","b"],[1,9],[3,4]]}}]}}"#),
    );
    let r = call(
        office__sheet_diff,
        &format!(r#"{{"left":"{a}","right":"{b}"}}"#),
    );
    assert_eq!(r["count"], 1, "one changed cell: {r}");
    assert_eq!(r["changed"][0]["ref"], "B2", "cell ref: {r}");
    assert_eq!(r["changed"][0]["left"], 2.0, "left val: {r}");
    assert_eq!(r["changed"][0]["right"], 9.0, "right val: {r}");

    for f in [&a, &b] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_reorder_workbook() {
    let path = tmp("reord.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"A","rows":[["x"]]}},{{"name":"B","rows":[["y"]]}},{{"name":"C","rows":[["z"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("reord_out.xlsx");
    let r = call(
        office__sheet_reorder,
        &format!(r#"{{"path":"{path}","order":["C","A","B"],"output":"{out}"}}"#),
    );
    assert_eq!(r["sheets"], 3, "three sheets: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let names: Vec<&str> = rd["sheets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["C", "A", "B"], "reordered: {rd}");

    for f in [&path, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_remove_one() {
    let path = tmp("rm.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"A","rows":[["x"],[1]]}},{{"name":"B","rows":[["y"],[2]]}},{{"name":"C","rows":[["z"],[3]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("rm_out.xlsx");
    let r = call(
        office__sheet_remove,
        &format!(r#"{{"path":"{path}","sheet":"B","output":"{out}"}}"#),
    );
    assert_eq!(r["removed"], "B", "removed B: {r}");
    assert_eq!(r["sheets"], 2, "two remain: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let names: Vec<&str> = rd["sheets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["A", "C"], "A and C remain: {rd}");

    for f in [&path, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_add_new() {
    let path = tmp("add.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"A","rows":[["x"],[1]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("add_out.xlsx");
    let r = call(
        office__sheet_add,
        &format!(r#"{{"path":"{path}","name":"B","rows":[["y"],[2]],"output":"{out}"}}"#),
    );
    assert_eq!(r["sheets"], 2, "two sheets: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(rd["sheets"][0]["name"], "A", "A kept: {rd}");
    assert_eq!(rd["sheets"][1]["name"], "B", "B added: {rd}");
    assert_eq!(rd["sheets"][1]["rows"][1][0], 2.0, "B data: {rd}");

    for f in [&path, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_copy_duplicates_worksheet() {
    let path = tmp("scopy.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"Tmpl","rows":[["x"],[1],[2]]}},{{"name":"Other","rows":[["y"],[9]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // copy Tmpl → "Tmpl 2", inserted right after the source (index 1)
    let out = tmp("scopy_out.xlsx");
    let r = call(
        office__sheet_copy,
        &format!(r#"{{"path":"{path}","sheet":"Tmpl","name":"Tmpl 2","output":"{out}"}}"#),
    );
    assert_eq!(r["sheets"], 3, "three sheets: {r}");
    assert_eq!(r["name"], "Tmpl 2", "copy name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(rd["sheets"][0]["name"], "Tmpl", "source kept: {rd}");
    assert_eq!(rd["sheets"][1]["name"], "Tmpl 2", "copy after source: {rd}");
    assert_eq!(rd["sheets"][2]["name"], "Other", "Other pushed down: {rd}");
    // copy carries the source data
    assert_eq!(rd["sheets"][1]["rows"][1][0], 1.0, "copy row1: {rd}");
    assert_eq!(rd["sheets"][1]["rows"][2][0], 2.0, "copy row2: {rd}");

    // name collision is rejected
    let dup = call(
        office__sheet_copy,
        &format!(r#"{{"path":"{path}","sheet":"Tmpl","name":"Other","output":"{out}"}}"#),
    );
    assert!(dup["error"].is_string(), "duplicate name rejected: {dup}");

    for f in [&path, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_rename_one() {
    let path = tmp("ren.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"Sheet1","rows":[["a"],[1]]}},{{"name":"X","rows":[["b"],[2]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("ren_out.xlsx");
    let r = call(
        office__sheet_rename,
        &format!(r#"{{"path":"{path}","from":"Sheet1","to":"Data","output":"{out}"}}"#),
    );
    assert_eq!(r["renamed"], "Data", "renamed: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(rd["sheets"][0]["name"], "Data", "first sheet renamed: {rd}");
    assert_eq!(rd["sheets"][1]["name"], "X", "other sheet preserved: {rd}");

    for f in [&path, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_top_by_column() {
    let path = tmp("top.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","sales"],["a",30],["b",10],["c",20]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("top_out.xlsx");
    let r = call(
        office__sheet_top,
        &format!(r#"{{"path":"{path}","by":"sales","n":2,"output":"{out}"}}"#),
    );
    assert_eq!(r["rows"], 2, "kept 2: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = &rd["sheets"][0]["rows"];
    assert_eq!(rows[1][0], "a", "highest first: {rd}");
    assert_eq!(rows[2][0], "c", "second highest: {rd}");

    for f in [&path, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_rank_competition_and_dense() {
    let path = tmp("rank.xlsx");
    // a=30, b=10, c=30 (tie with a), d=20
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","sales"],["a",30],["b",10],["c",30],["d",20]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // Competition ranking, largest first (default). Rows are NOT reordered.
    let out = tmp("rank_c.xlsx");
    let r = call(
        office__sheet_rank,
        &format!(r#"{{"path":"{path}","by":"sales","output":"{out}"}}"#),
    );
    assert_eq!(r["ranked"], 4, "four rows ranked: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][2], "rank", "header name appended: {rd}");
    assert_eq!(rows[1][0], "a", "row order preserved (a first): {rd}");
    // a=30 ->1, b=10 ->4, c=30 ->1 (tie), d=20 ->3 (competition skips 2)
    assert_eq!(rows[1][2].as_f64().unwrap(), 1.0, "a rank 1: {rd}");
    assert_eq!(rows[2][2].as_f64().unwrap(), 4.0, "b rank 4: {rd}");
    assert_eq!(rows[3][2].as_f64().unwrap(), 1.0, "c tie rank 1: {rd}");
    assert_eq!(rows[4][2].as_f64().unwrap(), 3.0, "d rank 3 (skip 2): {rd}");

    // Dense ranking: 30->1, 20->2, 10->3
    let outd = tmp("rank_d.xlsx");
    call(
        office__sheet_rank,
        &format!(r#"{{"path":"{path}","by":"sales","output":"{outd}","dense":true,"name":"pos"}}"#),
    );
    let rdd = call(office__sheet_read, &format!(r#"{{"path":"{outd}"}}"#));
    let rd2 = rdd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rd2[0][2], "pos", "custom name: {rdd}");
    assert_eq!(rd2[2][2].as_f64().unwrap(), 3.0, "b dense rank 3: {rdd}");
    assert_eq!(rd2[4][2].as_f64().unwrap(), 2.0, "d dense rank 2: {rdd}");

    for f in [&path, &out, &outd] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_pct_rank_cdf() {
    let path = tmp("pctrank.xlsx");
    // values 10,20,30,40 -> CDF (count<=)/n: 0.25, 0.5, 0.75, 1.0
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[10],[20],[30],[40]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("pctrank_out.xlsx");
    let r = call(
        office__sheet_pct_rank,
        &format!(r#"{{"path":"{path}","column":"v","output":"{out}"}}"#),
    );
    assert_eq!(r["column"], "v_pctrank", "default column name: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert!(
        (rows[1][1].as_f64().unwrap() - 0.25).abs() < 1e-9,
        "min -> 0.25: {rd}"
    );
    assert!(
        (rows[2][1].as_f64().unwrap() - 0.5).abs() < 1e-9,
        "second -> 0.5: {rd}"
    );
    assert!(
        (rows[4][1].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "max -> 1.0: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_head_and_tail() {
    let path = tmp("head.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["h"],[1],[2],[3],[4],[5]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // head 2 -> header + rows 1,2
    let h = tmp("head_h.xlsx");
    let rh = call(
        office__sheet_head,
        &format!(r#"{{"path":"{path}","n":2,"output":"{h}"}}"#),
    );
    assert_eq!(rh["rows"], 2, "kept 2: {rh}");
    let rdh = call(office__sheet_read, &format!(r#"{{"path":"{h}"}}"#));
    let rows = rdh["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "header + 2: {rdh}");
    assert_eq!(rows[1][0], 1.0, "first data: {rdh}");
    assert_eq!(rows[2][0], 2.0, "second data: {rdh}");

    // tail 2 -> header + rows 4,5
    let t = tmp("head_t.xlsx");
    call(
        office__sheet_head,
        &format!(r#"{{"path":"{path}","n":2,"tail":true,"output":"{t}"}}"#),
    );
    let rdt = call(office__sheet_read, &format!(r#"{{"path":"{t}"}}"#));
    let trows = rdt["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(trows[1][0], 4.0, "tail first: {rdt}");
    assert_eq!(trows[2][0], 5.0, "tail last: {rdt}");

    for f in [&path, &h, &t] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_sample_deterministic() {
    let path = tmp("sample.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["h"],[1],[2],[3],[4],[5],[6],[7],[8],[9],[10]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // sample 3 of 10 with a fixed seed -> header kept, count exact
    let o1 = tmp("sample1.xlsx");
    let r = call(
        office__sheet_sample,
        &format!(r#"{{"path":"{path}","n":3,"seed":42,"output":"{o1}"}}"#),
    );
    assert_eq!(r["rows"], 3, "sampled 3: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{o1}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4, "header + 3 sampled: {rd}");
    assert_eq!(rows[0][0], "h", "header preserved: {rd}");
    // sampled rows are emitted in ascending original order
    let v1 = rows[1][0].as_f64().unwrap();
    let v2 = rows[2][0].as_f64().unwrap();
    let v3 = rows[3][0].as_f64().unwrap();
    assert!(v1 < v2 && v2 < v3, "original order preserved: {rd}");

    // same seed -> identical sample (reproducible)
    let o2 = tmp("sample2.xlsx");
    call(
        office__sheet_sample,
        &format!(r#"{{"path":"{path}","n":3,"seed":42,"output":"{o2}"}}"#),
    );
    let rd2 = call(office__sheet_read, &format!(r#"{{"path":"{o2}"}}"#));
    assert_eq!(
        rd2["sheets"][0]["rows"], rd["sheets"][0]["rows"],
        "seed reproducible: {rd2}"
    );

    // n >= row count keeps all data rows
    let o3 = tmp("sample3.xlsx");
    let r3 = call(
        office__sheet_sample,
        &format!(r#"{{"path":"{path}","n":100,"output":"{o3}"}}"#),
    );
    assert_eq!(r3["rows"], 10, "n>=rows keeps all: {r3}");

    for f in [&path, &o1, &o2, &o3] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_stratified_sample_proportional() {
    let path = tmp("strat.xlsx");
    // 4 rows of group "a", 2 rows of group "b"
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[["g","v"],["a",1],["a",2],["a",3],["a",4],["b",5],["b",6]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // ratio 0.5: round(4*0.5)=2 from a, round(2*0.5)=1 from b -> 3 rows, 2 groups
    let out = tmp("strat_out.xlsx");
    let r = call(
        office__sheet_stratified_sample,
        &format!(r#"{{"path":"{path}","output":"{out}","group":"g","ratio":0.5,"seed":7}}"#),
    );
    assert_eq!(r["groups"], 2, "two groups: {r}");
    assert_eq!(r["rows"], 3, "2 from a + 1 from b: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    let a = rows[1..].iter().filter(|r| r[0] == "a").count();
    let b = rows[1..].iter().filter(|r| r[0] == "b").count();
    assert_eq!(a, 2, "two from a: {rd}");
    assert_eq!(b, 1, "one from b: {rd}");

    // determinism: same seed -> identical output
    let out2 = tmp("strat_out2.xlsx");
    call(
        office__sheet_stratified_sample,
        &format!(r#"{{"path":"{path}","output":"{out2}","group":"g","ratio":0.5,"seed":7}}"#),
    );
    let rd2 = call(office__sheet_read, &format!(r#"{{"path":"{out2}"}}"#));
    assert_eq!(
        rd["sheets"][0]["rows"], rd2["sheets"][0]["rows"],
        "same seed reproducible"
    );

    // n_per_group: fixed 1 per group -> 2 rows
    let out3 = tmp("strat_out3.xlsx");
    let r3 = call(
        office__sheet_stratified_sample,
        &format!(r#"{{"path":"{path}","output":"{out3}","group":"g","n_per_group":1,"seed":7}}"#),
    );
    assert_eq!(r3["rows"], 2, "1 per group x2: {r3}");

    for f in [&path, &out, &out2, &out3] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_shuffle_reproducible() {
    let path = tmp("shuffle.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["h"],[1],[2],[3],[4],[5],[6],[7],[8]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let shuf = |seed: u64, out: &str| {
        let r = call(
            office__sheet_shuffle,
            &format!(r#"{{"path":"{path}","seed":{seed},"output":"{out}"}}"#),
        );
        assert_eq!(r["rows"], 8, "all 8 data rows kept: {r}");
        let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
        let rows = rd["sheets"][0]["rows"].as_array().unwrap();
        assert_eq!(rows[0][0], "h", "header stays on top: {rd}");
        // collect the shuffled data values
        rows[1..]
            .iter()
            .map(|r| r[0].as_f64().unwrap() as i64)
            .collect::<Vec<_>>()
    };

    let (pa, pb, pc) = (tmp("shuf_a.xlsx"), tmp("shuf_b.xlsx"), tmp("shuf_c.xlsx"));
    let a = shuf(7, &pa);
    let b = shuf(7, &pb);
    let c = shuf(99, &pc);
    assert_eq!(a, b, "same seed -> same permutation");
    // it's a permutation of 1..=7
    let mut sorted = a.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        vec![1, 2, 3, 4, 5, 6, 7, 8],
        "all rows present once"
    );
    // a different seed should (very likely) give a different order
    assert_ne!(a, c, "different seed -> different order");

    for f in [&path, &pa, &pb, &pc] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_train_test_split_ratio() {
    let path = tmp("tts.xlsx");
    // 10 data rows; 0.7 ratio -> 7 train, 3 test
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[1],[2],[3],[4],[5],[6],[7],[8],[9],[10]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let tr = tmp("tts_train.xlsx");
    let te = tmp("tts_test.xlsx");
    let r = call(
        office__sheet_train_test_split,
        &format!(r#"{{"path":"{path}","train":"{tr}","test":"{te}","ratio":0.7,"seed":7}}"#),
    );
    assert_eq!(r["train_rows"], 7, "70% to train: {r}");
    assert_eq!(r["test_rows"], 3, "30% to test: {r}");
    // each file keeps the header; partition is disjoint and complete
    let rdt = call(office__sheet_read, &format!(r#"{{"path":"{tr}"}}"#));
    let rde = call(office__sheet_read, &format!(r#"{{"path":"{te}"}}"#));
    let trows = rdt["sheets"][0]["rows"].as_array().unwrap();
    let erows = rde["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(trows[0][0], "v", "train header: {rdt}");
    assert_eq!(erows[0][0], "v", "test header: {rde}");
    assert_eq!(trows.len(), 8, "header + 7 train: {rdt}");
    assert_eq!(erows.len(), 4, "header + 3 test: {rde}");
    // union of values is the full 1..=10 set
    let mut all: Vec<i64> = trows[1..]
        .iter()
        .chain(erows[1..].iter())
        .map(|r| r[0].as_f64().unwrap() as i64)
        .collect();
    all.sort_unstable();
    assert_eq!(
        all,
        (1..=10).collect::<Vec<_>>(),
        "disjoint + complete: {rdt} {rde}"
    );

    for f in [&path, &tr, &te] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_transform_column_ops() {
    let path = tmp("xform.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["name","price"],
                ["  apple ",1.234],
                ["BANANA",2.567]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // trim "name" in place
    let o1 = tmp("xform1.xlsx");
    let r = call(
        office__sheet_transform,
        &serde_json::json!({ "path": path, "column": "name", "op": "trim", "output": o1 })
            .to_string(),
    );
    assert_eq!(r["transformed"], 2, "two data rows: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{o1}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][0], "apple", "trimmed in place: {rd}");

    // round "price" to 1 decimal into a new column
    let o2 = tmp("xform2.xlsx");
    call(
        office__sheet_transform,
        &serde_json::json!({
            "path": path, "column": "price", "op": "round", "digits": 1,
            "into": "rounded", "output": o2
        })
        .to_string(),
    );
    let rd2 = call(office__sheet_read, &format!(r#"{{"path":"{o2}"}}"#));
    let rows2 = rd2["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows2[0][2], "rounded", "new column header: {rd2}");
    assert!(
        (rows2[1][2].as_f64().unwrap() - 1.2).abs() < 1e-9,
        "1.234->1.2: {rd2}"
    );
    assert!(
        (rows2[2][2].as_f64().unwrap() - 2.6).abs() < 1e-9,
        "2.567->2.6: {rd2}"
    );
    // original price column untouched
    assert!(
        (rows2[1][1].as_f64().unwrap() - 1.234).abs() < 1e-9,
        "original kept: {rd2}"
    );

    // upper op
    let o3 = tmp("xform3.xlsx");
    call(
        office__sheet_transform,
        &serde_json::json!({ "path": path, "column": "name", "op": "upper", "output": o3 })
            .to_string(),
    );
    let rd3 = call(office__sheet_read, &format!(r#"{{"path":"{o3}"}}"#));
    assert_eq!(rd3["sheets"][0]["rows"][2][0], "BANANA", "upper: {rd3}");

    for f in [&path, &o1, &o2, &o3] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_cast_messy_numbers() {
    let path = tmp("cast.xlsx");
    // messy money strings + accounting negative + percent
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["amt"],
                ["$1,234.50"],
                ["(2,000)"],
                ["45%"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("cast_out.xlsx");
    let r = call(
        office__sheet_cast,
        &format!(r#"{{"path":"{path}","output":"{out}","by":"amt","type":"number"}}"#),
    );
    assert_eq!(r["cast"], 3, "three cells cast: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert!(
        (rows[1][0].as_f64().unwrap() - 1234.5).abs() < 1e-9,
        "$1,234.50: {rd}"
    );
    assert!(
        (rows[2][0].as_f64().unwrap() + 2000.0).abs() < 1e-9,
        "(2,000)->-2000: {rd}"
    );
    assert!(
        (rows[3][0].as_f64().unwrap() - 45.0).abs() < 1e-9,
        "45%->45: {rd}"
    );

    // int cast truncates
    let outi = tmp("cast_int.xlsx");
    call(
        office__sheet_cast,
        &format!(r#"{{"path":"{path}","output":"{outi}","by":"amt","type":"int"}}"#),
    );
    let rdi = call(office__sheet_read, &format!(r#"{{"path":"{outi}"}}"#));
    assert_eq!(
        rdi["sheets"][0]["rows"][1][0].as_f64().unwrap(),
        1234.0,
        "int trunc: {rdi}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outi).ok();
}

#[test]
fn sheet_strip_whitespace() {
    let path = tmp("strip.xlsx");
    let w = call(
        office__sheet_write,
        &serde_json::json!({
            "path": path,
            "sheets": [{
                "name": "D",
                "rows": [["  name ", "city"], [" apple ", "new   york"], [5, " keep "]]
            }]
        })
        .to_string(),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // default: trim ends only
    let out = tmp("strip_out.xlsx");
    let r = call(
        office__sheet_strip,
        &format!(r#"{{"path":"{path}","output":"{out}"}}"#),
    );
    assert!(r["trimmed"].as_u64().unwrap() >= 3, "trimmed count: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "name", "header trimmed: {rd}");
    assert_eq!(rows[1][0], "apple", "cell trimmed: {rd}");
    assert_eq!(rows[2][1], "keep", "trailing-col cell trimmed: {rd}");
    // numeric untouched
    assert_eq!(rows[2][0].as_f64().unwrap(), 5.0, "numeric kept: {rd}");
    // internal whitespace preserved without collapse
    assert_eq!(rows[1][1], "new   york", "internal whitespace kept: {rd}");

    // collapse mode squeezes internal runs
    let outc = tmp("strip_col.xlsx");
    call(
        office__sheet_strip,
        &format!(r#"{{"path":"{path}","output":"{outc}","collapse":true}}"#),
    );
    let rdc = call(office__sheet_read, &format!(r#"{{"path":"{outc}"}}"#));
    assert_eq!(
        rdc["sheets"][0]["rows"][1][1], "new york",
        "collapsed internal whitespace: {rdc}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outc).ok();
}

#[test]
fn sheet_pad_fixed_width() {
    let path = tmp("pad.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["id"],["5"],["42"],["12345"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // zero-pad to width 4, left (default)
    let out = tmp("pad_out.xlsx");
    let r = call(
        office__sheet_pad,
        &format!(r#"{{"path":"{path}","output":"{out}","column":"id","width":4}}"#),
    );
    assert_eq!(r["padded"], 2, "two shorter values padded: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][0], "0005", "5 -> 0005: {rd}");
    assert_eq!(rows[2][0], "0042", "42 -> 0042: {rd}");
    assert_eq!(rows[3][0], "12345", "already wide -> unchanged: {rd}");

    // right side pad with a custom fill into a new column
    let outr = tmp("pad_r.xlsx");
    call(
        office__sheet_pad,
        &serde_json::json!({
            "path": path, "output": outr, "column": "id",
            "width": 3, "fill": ".", "side": "right", "into": "padded"
        })
        .to_string(),
    );
    let rdr = call(office__sheet_read, &format!(r#"{{"path":"{outr}"}}"#));
    let rowsr = rdr["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rowsr[0][1], "padded", "new column header: {rdr}");
    assert_eq!(rowsr[1][1], "5..", "5 -> 5.. (right): {rdr}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outr).ok();
}

#[test]
fn sheet_substr_extract() {
    let path = tmp("substr.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["code"],["ABC123"],["XYZ999"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // first 3 chars into a new column
    let out = tmp("substr_out.xlsx");
    let r = call(
        office__sheet_substr,
        &format!(
            r#"{{"path":"{path}","output":"{out}","column":"code","start":0,"len":3,"into":"prefix"}}"#
        ),
    );
    assert_eq!(r["column"], "prefix", "new column: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][1], "prefix", "header appended: {rd}");
    assert_eq!(rows[1][1], "ABC", "ABC123 -> ABC: {rd}");
    assert_eq!(rows[2][1], "XYZ", "XYZ999 -> XYZ: {rd}");

    // negative start: last 3 chars, in place
    let outn = tmp("substr_neg.xlsx");
    call(
        office__sheet_substr,
        &format!(r#"{{"path":"{path}","output":"{outn}","column":"code","start":-3}}"#),
    );
    let rdn = call(office__sheet_read, &format!(r#"{{"path":"{outn}"}}"#));
    assert_eq!(rdn["sheets"][0]["rows"][1][0], "123", "last 3 chars: {rdn}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outn).ok();
}

#[test]
fn sheet_extract_regex() {
    let path = tmp("extract.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["email"],["bob@acme.com"],["sue@x.org"],["nope"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // capture the domain (group 1)
    let out = tmp("extract_out.xlsx");
    let r = call(
        office__sheet_extract,
        &serde_json::json!({
            "path": path, "output": out, "column": "email",
            "pattern": "@(.+)$", "group": 1, "into": "domain"
        })
        .to_string(),
    );
    assert_eq!(r["column"], "domain", "new column: {r}");
    assert_eq!(r["matched"], 2, "two emails matched: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][1], "acme.com", "domain extracted: {rd}");
    assert_eq!(rows[2][1], "x.org", "second domain: {rd}");
    assert_eq!(
        rows[3][1].as_str().unwrap_or(""),
        "",
        "no match -> blank: {rd}"
    );

    // invalid regex errors
    let bad = call(
        office__sheet_extract,
        &format!(
            r#"{{"path":"{path}","output":"{out}","column":"email","pattern":"([","into":"x"}}"#
        ),
    );
    assert!(bad["error"].is_string(), "invalid regex rejected: {bad}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_grep_regex_rows() {
    let path = tmp("sgrep.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["name","code"],
                ["alpha","A12"],
                ["beta","B7"],
                ["gamma","A99"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // codes starting with A followed by two digits -> alpha, gamma
    let out = tmp("sgrep_out.xlsx");
    let r = call(
        office__sheet_grep,
        &format!(r#"{{"path":"{path}","output":"{out}","column":"code","pattern":"^A\\d\\d$"}}"#),
    );
    assert_eq!(r["kept"], 2, "two rows match: {r}");
    assert_eq!(r["removed"], 1, "one removed: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "header + 2 rows: {rd}");
    assert_eq!(rows[1][0], "alpha", "first match: {rd}");
    assert_eq!(rows[2][0], "gamma", "second match: {rd}");

    // any-cell match (no column) + invert
    let outi = tmp("sgrep_inv.xlsx");
    let ri = call(
        office__sheet_grep,
        &format!(r#"{{"path":"{path}","output":"{outi}","pattern":"beta","invert":true}}"#),
    );
    assert_eq!(ri["kept"], 2, "invert drops the beta row: {ri}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outi).ok();
}

#[test]
fn sheet_reverse_data_rows() {
    let path = tmp("reverse.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["n"],[1],[2],[3]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("reverse_out.xlsx");
    let r = call(
        office__sheet_reverse,
        &format!(r#"{{"path":"{path}","output":"{out}"}}"#),
    );
    assert_eq!(r["rows"], 3, "three data rows: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "n", "header stays on top: {rd}");
    assert_eq!(rows[1][0].as_f64().unwrap(), 3.0, "first data now 3: {rd}");
    assert_eq!(rows[3][0].as_f64().unwrap(), 1.0, "last data now 1: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_coalesce_first_nonblank() {
    let path = tmp("coalesce.xlsx");
    // primary blank in row1, present in row2; secondary fallback used in row1.
    let w = call(
        office__sheet_write,
        &serde_json::json!({
            "path": path,
            "sheets": [{
                "name": "D",
                "rows": [
                    ["primary", "fallback", "keep"],
                    ["", "alt1", "k1"],
                    ["main2", "alt2", "k2"],
                    ["", "", "k3"]
                ]
            }]
        })
        .to_string(),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("coalesce_out.xlsx");
    let r = call(
        office__sheet_coalesce,
        &serde_json::json!({
            "path": path, "output": out,
            "columns": ["primary", "fallback"], "into": "best", "default": "N/A"
        })
        .to_string(),
    );
    assert_eq!(r["column"], "best", "new column name: {r}");
    assert_eq!(r["filled"], 2, "two rows had a non-blank: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    // "best" is appended after the three source columns -> index 3
    assert_eq!(rows[0][3], "best", "header appended: {rd}");
    assert_eq!(rows[1][3], "alt1", "row1 falls back to secondary: {rd}");
    assert_eq!(rows[2][3], "main2", "row2 uses primary: {rd}");
    assert_eq!(rows[3][3], "N/A", "all-blank row gets default: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_recode_value_map() {
    let path = tmp("recode.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["flag"],["Y"],["N"],["Y"],["?"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // recode in place with a default for the unmapped "?"
    let out = tmp("recode_out.xlsx");
    let r = call(
        office__sheet_recode,
        &serde_json::json!({
            "path": path, "output": out, "column": "flag",
            "map": { "Y": "Yes", "N": "No" }, "default": "Unknown"
        })
        .to_string(),
    );
    assert_eq!(r["recoded"], 3, "three mapped cells: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][0], "Yes", "Y -> Yes: {rd}");
    assert_eq!(rows[2][0], "No", "N -> No: {rd}");
    assert_eq!(rows[4][0], "Unknown", "? -> default: {rd}");

    // into a new column, no default → unmapped kept as-is
    let outc = tmp("recode_into.xlsx");
    call(
        office__sheet_recode,
        &serde_json::json!({
            "path": path, "output": outc, "column": "flag",
            "map": { "Y": "Yes" }, "into": "label"
        })
        .to_string(),
    );
    let rdc = call(office__sheet_read, &format!(r#"{{"path":"{outc}"}}"#));
    let rowsc = rdc["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rowsc[0][1], "label", "new column header: {rdc}");
    assert_eq!(rowsc[1][1], "Yes", "mapped in new col: {rdc}");
    assert_eq!(rowsc[2][1], "N", "unmapped kept (no default): {rdc}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outc).ok();
}

#[test]
fn sheet_chunk_rows() {
    let path = tmp("schunk.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["h"],[1],[2],[3],[4],[5]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let dir = tmp("schunk_out");
    std::fs::create_dir_all(&dir).unwrap();
    let r = call(
        office__sheet_chunk,
        &format!(r#"{{"path":"{path}","size":2,"dir":"{dir}","prefix":"c"}}"#),
    );
    assert_eq!(r["count"], 3, "5 data rows / 2 -> 3 chunks: {r}");
    let c1 = call(
        office__sheet_read,
        &format!(r#"{{"path":"{dir}/c-1.xlsx"}}"#),
    );
    let r1 = c1["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(r1.len(), 3, "header + 2 rows: {c1}");
    assert_eq!(r1[0][0], "h", "header repeated: {c1}");
    assert_eq!(r1[1][0], 1.0, "first data: {c1}");
    let c3 = call(
        office__sheet_read,
        &format!(r#"{{"path":"{dir}/c-3.xlsx"}}"#),
    );
    assert_eq!(
        c3["sheets"][0]["rows"].as_array().unwrap().len(),
        2,
        "header + 1 row: {c3}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sheet_split_per_sheet() {
    let path = tmp("wb.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[
                {{"name":"Alpha","rows":[["a"],[1]]}},
                {{"name":"Beta","rows":[["b"],[2]]}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let dir = tmp("wb_out");
    std::fs::create_dir_all(&dir).unwrap();
    let r = call(
        office__sheet_split,
        &format!(r#"{{"path":"{path}","dir":"{dir}"}}"#),
    );
    assert_eq!(r["count"], 2, "two per-sheet files: {r}");
    let a = call(
        office__sheet_read,
        &format!(r#"{{"path":"{dir}/Alpha.xlsx"}}"#),
    );
    assert_eq!(a["sheets"][0]["rows"][0][0], "a", "Alpha file content: {a}");
    let b = call(
        office__sheet_read,
        &format!(r#"{{"path":"{dir}/Beta.xlsx"}}"#),
    );
    assert_eq!(b["sheets"][0]["rows"][1][0], 2.0, "Beta file value: {b}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sheet_fill_blanks() {
    let path = tmp("fill.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","amt"],
                ["west",10],
                ["",20],
                ["east",30],
                ["",40]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // ffill the region column
    let out = tmp("fill_ff.xlsx");
    let r = call(
        office__sheet_fill,
        &format!(r#"{{"path":"{path}","by":"region","output":"{out}"}}"#),
    );
    assert_eq!(r["filled"], 2, "two blanks filled: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = &rd["sheets"][0]["rows"];
    assert_eq!(rows[2][0], "west", "row2 ffilled from west: {rd}");
    assert_eq!(rows[4][0], "east", "row4 ffilled from east: {rd}");

    // constant fill of all blanks
    let outv = tmp("fill_v.xlsx");
    let rv = call(
        office__sheet_fill,
        &format!(r#"{{"path":"{path}","method":"value","value":"NA","output":"{outv}"}}"#),
    );
    assert_eq!(rv["filled"], 2, "two blanks set to NA: {rv}");
    let rdv = call(office__sheet_read, &format!(r#"{{"path":"{outv}"}}"#));
    assert_eq!(rdv["sheets"][0]["rows"][2][0], "NA", "blank -> NA: {rdv}");

    // bfill the region column: row2 blank (between west and east) -> "east"
    let outb = tmp("fill_bf.xlsx");
    let rb = call(
        office__sheet_fill,
        &format!(r#"{{"path":"{path}","by":"region","method":"bfill","output":"{outb}"}}"#),
    );
    // rows: header, west, "", east, "" -> the "" before east backfills; the
    // trailing "" has no value below it, so only one blank fills.
    assert_eq!(rb["filled"], 1, "bfill fills the inner blank: {rb}");
    let rdb = call(office__sheet_read, &format!(r#"{{"path":"{outb}"}}"#));
    let rowsb = rdb["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(
        rowsb[2][0], "east",
        "inner blank backfilled from east: {rdb}"
    );

    for f in [&path, &out, &outv, &outb] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_impute_statistics() {
    let path = tmp("impute.xlsx");
    // column v: 2, blank, 4, 6 -> mean 4, median 4
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[["v"],[2],[null],[4],[6]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // mean imputation
    let outm = tmp("impute_mean.xlsx");
    let rm = call(
        office__sheet_impute,
        &format!(r#"{{"path":"{path}","output":"{outm}","strategy":"mean","by":"v"}}"#),
    );
    assert_eq!(rm["filled"].as_u64().unwrap(), 1, "one blank filled: {rm}");
    assert_eq!(rm["columns"].as_u64().unwrap(), 1, "one column: {rm}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{outm}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[2][0].as_f64().unwrap(), 4.0, "mean of 2,4,6 = 4: {rd}");

    // median imputation
    let outd = tmp("impute_median.xlsx");
    call(
        office__sheet_impute,
        &format!(r#"{{"path":"{path}","output":"{outd}","strategy":"median","by":"v"}}"#),
    );
    let rdd = call(office__sheet_read, &format!(r#"{{"path":"{outd}"}}"#));
    assert_eq!(
        rdd["sheets"][0]["rows"][2][0].as_f64().unwrap(),
        4.0,
        "median of 2,4,6 = 4: {rdd}"
    );

    // zero imputation
    let outz = tmp("impute_zero.xlsx");
    call(
        office__sheet_impute,
        &format!(r#"{{"path":"{path}","output":"{outz}","strategy":"zero","by":"v"}}"#),
    );
    let rdz = call(office__sheet_read, &format!(r#"{{"path":"{outz}"}}"#));
    assert_eq!(
        rdz["sheets"][0]["rows"][2][0].as_f64().unwrap(),
        0.0,
        "zero fill: {rdz}"
    );

    // mode imputation on a categorical column
    let cpath = tmp("impute_cat.xlsx");
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{cpath}","sheets":[{{"name":"S","rows":[["c"],["x"],["x"],[null],["y"]]}}]}}"#
        ),
    );
    let outc = tmp("impute_mode.xlsx");
    call(
        office__sheet_impute,
        &format!(r#"{{"path":"{cpath}","output":"{outc}","strategy":"mode","by":"c"}}"#),
    );
    let rdc = call(office__sheet_read, &format!(r#"{{"path":"{outc}"}}"#));
    assert_eq!(
        rdc["sheets"][0]["rows"][3][0], "x",
        "mode is most frequent 'x': {rdc}"
    );

    for f in [&path, &outm, &outd, &outz, &cpath, &outc] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_interpolate_linear() {
    let path = tmp("interp.xlsx");
    // gap of two blanks between 10 and 40 → 20, 30; a trailing gap stays blank.
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["v"],
                [10],
                [""],
                [""],
                [40],
                [""]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("interp_out.xlsx");
    let r = call(
        office__sheet_interpolate,
        &format!(r#"{{"path":"{path}","by":"v","output":"{out}"}}"#),
    );
    assert_eq!(r["filled"], 2, "two internal blanks filled: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = &rd["sheets"][0]["rows"];
    assert_eq!(rows[2][0].as_f64().unwrap(), 20.0, "interp 1/3 -> 20: {rd}");
    assert_eq!(rows[3][0].as_f64().unwrap(), 30.0, "interp 2/3 -> 30: {rd}");
    // trailing gap (no right neighbor) left blank — never a number
    assert!(
        rows[5][0].as_f64().is_none(),
        "trailing gap not filled: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_append_rows_and_records() {
    let path = tmp("app.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","qty"],["a",1]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // append raw rows
    let out = tmp("app_rows.xlsx");
    let r = call(
        office__sheet_append,
        &format!(r#"{{"path":"{path}","rows":[["b",2],["c",3]],"output":"{out}"}}"#),
    );
    assert_eq!(r["added"], 2, "added 2: {r}");
    assert_eq!(r["rows"], 4, "4 total rows: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(rd["sheets"][0]["rows"][3][0], "c", "appended row c: {rd}");

    // append records mapped to header (note column order honored)
    let out2 = tmp("app_recs.xlsx");
    let r2 = call(
        office__sheet_append,
        &format!(r#"{{"path":"{path}","records":[{{"qty":9,"name":"z"}}],"output":"{out2}"}}"#),
    );
    assert_eq!(r2["added"], 1, "added 1 record: {r2}");
    let rd2 = call(office__sheet_read, &format!(r#"{{"path":"{out2}"}}"#));
    let last = &rd2["sheets"][0]["rows"][2];
    assert_eq!(last[0], "z", "record name -> col0: {rd2}");
    assert_eq!(last[1], 9.0, "record qty -> col1: {rd2}");

    for f in [&path, &out, &out2] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_hstack_side_by_side() {
    let left = tmp("hl.xlsx");
    let right = tmp("hr.xlsx");
    let wl = call(
        office__sheet_write,
        &format!(r#"{{"path":"{left}","sheets":[{{"name":"L","rows":[["a","b"],[1,2],[3,4]]}}]}}"#),
    );
    assert_eq!(wl["ok"], true, "write l: {wl}");
    // right has fewer rows -> shorter side padded
    let wr = call(
        office__sheet_write,
        &format!(r#"{{"path":"{right}","sheets":[{{"name":"R","rows":[["c"],[9]]}}]}}"#),
    );
    assert_eq!(wr["ok"], true, "write r: {wr}");

    let out = tmp("hstack_out.xlsx");
    let r = call(
        office__sheet_hstack,
        &format!(r#"{{"path":"{left}","right":"{right}","output":"{out}"}}"#),
    );
    assert_eq!(r["rows"], 3, "max of 3 and 2 rows: {r}");
    assert_eq!(r["columns"], 3, "2 + 1 columns: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "a", "left header: {rd}");
    assert_eq!(rows[0][2], "c", "right header appended: {rd}");
    assert_eq!(rows[1][1].as_f64().unwrap(), 2.0, "left row1 b: {rd}");
    assert_eq!(rows[1][2].as_f64().unwrap(), 9.0, "right row1 c: {rd}");
    // row index 2 exists on left only; right side padded blank
    assert_eq!(rows[2][0].as_f64().unwrap(), 3.0, "left row2: {rd}");
    assert!(rows[2][2].as_f64().is_none(), "right padded blank: {rd}");

    std::fs::remove_file(&left).ok();
    std::fs::remove_file(&right).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_cross_cartesian() {
    let left = tmp("cl.xlsx");
    let right = tmp("cr.xlsx");
    // sizes (2) × colors (3) -> 6 combinations
    call(
        office__sheet_write,
        &format!(r#"{{"path":"{left}","sheets":[{{"name":"L","rows":[["size"],["S"],["M"]]}}]}}"#),
    );
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{right}","sheets":[{{"name":"R","rows":[["color"],["red"],["green"],["blue"]]}}]}}"#
        ),
    );

    let out = tmp("cross_out.xlsx");
    let r = call(
        office__sheet_cross,
        &format!(r#"{{"path":"{left}","right":"{right}","output":"{out}"}}"#),
    );
    assert_eq!(r["rows"], 6, "2 x 3 = 6 combinations: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 7, "header + 6 rows: {rd}");
    assert_eq!(rows[0][0], "size", "left header: {rd}");
    assert_eq!(rows[0][1], "color", "right header joined: {rd}");
    assert_eq!(rows[1][0], "S", "first combo size: {rd}");
    assert_eq!(rows[1][1], "red", "first combo color: {rd}");

    std::fs::remove_file(&left).ok();
    std::fs::remove_file(&right).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_dedupe_rows() {
    let path = tmp("dedup.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["name","qty"],
                ["a",1],
                ["b",2],
                ["a",1],
                ["a",3]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // whole-row dedupe: the duplicate a/1 drops -> 3 kept
    let wr = tmp("dedup_row.xlsx");
    let r = call(
        office__sheet_dedupe,
        &format!(r#"{{"path":"{path}","output":"{wr}"}}"#),
    );
    assert_eq!(r["kept"], 3, "whole-row kept 3: {r}");
    assert_eq!(r["removed"], 1, "removed 1: {r}");

    // by name, keep first -> a(1), b(2)
    let bf = tmp("dedup_bf.xlsx");
    let r2 = call(
        office__sheet_dedupe,
        &format!(r#"{{"path":"{path}","by":"name","output":"{bf}"}}"#),
    );
    assert_eq!(r2["kept"], 2, "by-name kept 2: {r2}");
    let rb = call(office__sheet_read, &format!(r#"{{"path":"{bf}"}}"#));
    assert_eq!(rb["sheets"][0]["rows"][1][1], 1.0, "keep first qty=1: {rb}");

    // by name, keep last -> a(3), b(2)
    let bl = tmp("dedup_bl.xlsx");
    let r3 = call(
        office__sheet_dedupe,
        &format!(r#"{{"path":"{path}","by":"name","keep":"last","output":"{bl}"}}"#),
    );
    assert_eq!(r3["kept"], 2, "keep-last kept 2: {r3}");
    let rl = call(office__sheet_read, &format!(r#"{{"path":"{bl}"}}"#));
    let rows = rl["sheets"][0]["rows"].as_array().unwrap();
    let a_qty = rows
        .iter()
        .skip(1)
        .find(|r| r[0] == "a")
        .map(|r| r[1].clone())
        .unwrap();
    assert_eq!(a_qty, 3.0, "keep last a qty=3: {rl}");

    for f in [&path, &wr, &bf, &bl] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_duplicates_by_key() {
    let path = tmp("dups.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["name","qty"],
                ["a",1],
                ["b",2],
                ["a",1],
                ["a",3]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // by name: "a" appears at data rows 0,2,3 (3x); "b" once → one dup group
    let r = call(
        office__sheet_duplicates,
        &format!(r#"{{"path":"{path}","by":"name"}}"#),
    );
    assert_eq!(r["duplicates"], 2, "two redundant rows (3 a's): {r}");
    let groups = r["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "one duplicated key: {r}");
    assert_eq!(groups[0]["key"], "a", "duplicated key: {r}");
    assert_eq!(groups[0]["count"], 3, "three occurrences: {r}");
    let idxs: Vec<u64> = groups[0]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert_eq!(idxs, vec![0, 2, 3], "0-based data-row indices: {r}");

    // whole-row: only a/1 (rows 0,2) is an exact duplicate
    let rw = call(office__sheet_duplicates, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(rw["duplicates"], 1, "one exact-row duplicate: {rw}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_transpose_swaps_axes() {
    let path = tmp("trans.xlsx");
    // 2 rows x 3 cols
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["a","b","c"],[1,2,3]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("trans_out.xlsx");
    let r = call(
        office__sheet_transpose,
        &format!(r#"{{"path":"{path}","output":"{out}"}}"#),
    );
    assert_eq!(r["rows"], 3, "3 rows after transpose: {r}");
    assert_eq!(r["columns"], 2, "2 columns after transpose: {r}");
    let rr = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = &rr["sheets"][0]["rows"];
    assert_eq!(rows[0][0], "a", "first cell: {rr}");
    assert_eq!(rows[0][1], 1.0, "transposed value: {rr}");
    assert_eq!(rows[2][0], "c", "last row key: {rr}");
    assert_eq!(rows[2][1], 3.0, "last row value: {rr}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_replace_values() {
    let path = tmp("srep.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","note"],["Alice","hello world"],["bob","HELLO there"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("srep_out.xlsx");
    let r = call(
        office__sheet_replace,
        &format!(
            r#"{{"path":"{path}","find":"hello","replace":"hi","ignore_case":true,"output":"{out}"}}"#
        ),
    );
    assert_eq!(r["replaced"], 2, "two replacements: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = &rd["sheets"][0]["rows"];
    assert_eq!(rows[1][1], "hi world", "row1 note: {rd}");
    assert_eq!(rows[2][1], "hi there", "row2 note (ci): {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_replace_regex_captures() {
    let path = tmp("rxrepl.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["phone"],["555-1234"],["867-5309"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // regex with capture-group substitution: reformat NNN-NNNN -> (NNN) NNNN
    let out = tmp("rxrepl_out.xlsx");
    let r = call(
        office__sheet_replace,
        &format!(
            r#"{{"path":"{path}","find":"(\\d{{3}})-(\\d{{4}})","replace":"($1) $2","regex":true,"output":"{out}"}}"#
        ),
    );
    assert_eq!(r["replaced"].as_u64().unwrap(), 2, "two cells matched: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][0], "(555) 1234", "capture reformat: {rd}");
    assert_eq!(rows[2][0], "(867) 5309", "capture reformat 2: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_totals_row() {
    let path = tmp("tot.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["item","qty","price"],["a",2,10],["b",3,20]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("tot_out.xlsx");
    let r = call(
        office__sheet_totals,
        &format!(r#"{{"path":"{path}","output":"{out}"}}"#),
    );
    assert_eq!(r["totals"], 2, "two numeric columns summed: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    let last = rows.last().unwrap();
    assert_eq!(last[0], "Total", "label: {rd}");
    assert_eq!(last[1], 5.0, "qty total: {rd}");
    assert_eq!(last[2], 30.0, "price total: {rd}");

    for f in [&path, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_subtotal_by_group() {
    let path = tmp("subtot.xlsx");
    // grouped by region (sorted): east(10,20), west(30)
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","amt"],
                ["east",10],
                ["east",20],
                ["west",30]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("subtot_out.xlsx");
    let r = call(
        office__sheet_subtotal,
        &format!(r#"{{"path":"{path}","output":"{out}","group":"region","value":"amt"}}"#),
    );
    assert_eq!(r["groups"], 2, "two groups: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    // header, east 10, east 20, "east Total" 30, west 30, "west Total" 30, "Grand Total" 60
    assert_eq!(rows.len(), 7, "rows incl subtotals + grand: {rd}");
    assert_eq!(rows[3][0], "east Total", "east subtotal label: {rd}");
    assert_eq!(
        rows[3][1].as_f64().unwrap(),
        30.0,
        "east subtotal sum: {rd}"
    );
    assert_eq!(rows[5][0], "west Total", "west subtotal label: {rd}");
    assert_eq!(
        rows[5][1].as_f64().unwrap(),
        30.0,
        "west subtotal sum: {rd}"
    );
    assert_eq!(rows[6][0], "Grand Total", "grand total label: {rd}");
    assert_eq!(rows[6][1].as_f64().unwrap(), 60.0, "grand total sum: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_add_column_derived() {
    let path = tmp("addcol.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["first","last"],["John","Doe"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // concat first + last -> full
    let c = tmp("addcol_c.xlsx");
    let rc = call(
        office__sheet_add_column,
        &format!(r#"{{"path":"{path}","name":"full","concat":["first","last"],"output":"{c}"}}"#),
    );
    assert_eq!(rc["column"], "full", "added full: {rc}");
    let rdc = call(office__sheet_read, &format!(r#"{{"path":"{c}"}}"#));
    let rows = &rdc["sheets"][0]["rows"];
    assert_eq!(rows[0][2], "full", "header: {rdc}");
    assert_eq!(rows[1][2], "John Doe", "concatenated: {rdc}");

    // constant column
    let k = tmp("addcol_k.xlsx");
    call(
        office__sheet_add_column,
        &format!(r#"{{"path":"{path}","name":"src","value":"import","output":"{k}"}}"#),
    );
    let rdk = call(office__sheet_read, &format!(r#"{{"path":"{k}"}}"#));
    assert_eq!(rdk["sheets"][0]["rows"][1][2], "import", "constant: {rdk}");

    for f in [&path, &c, &k] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_drop_columns() {
    let path = tmp("drop.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["a","b","c"],[1,2,3]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("drop_out.xlsx");
    let r = call(
        office__sheet_drop,
        &format!(r#"{{"path":"{path}","columns":["b"],"output":"{out}"}}"#),
    );
    assert_eq!(r["columns"], 2, "two columns kept: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = &rd["sheets"][0]["rows"];
    assert_eq!(rows[0][0], "a", "kept a: {rd}");
    assert_eq!(rows[0][1], "c", "kept c (b dropped): {rd}");
    assert_eq!(rows[1][1], 3.0, "value under c: {rd}");

    for f in [&path, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_select_columns() {
    let path = tmp("sel.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","qty","note"],["a",10,"x"],["b",20,"y"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // reorder + drop: keep note, name
    let out = tmp("sel_out.xlsx");
    let r = call(
        office__sheet_select,
        &format!(r#"{{"path":"{path}","columns":["note","name"],"output":"{out}"}}"#),
    );
    assert_eq!(r["columns"], 2, "two columns: {r}");
    let rr = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = &rr["sheets"][0]["rows"];
    assert_eq!(rows[0][0], "note", "header reordered: {rr}");
    assert_eq!(rows[0][1], "name");
    assert_eq!(rows[1][0], "x", "row cell reordered: {rr}");
    assert_eq!(rows[1][1], "a");

    // select by index
    let iout = tmp("sel_idx.xlsx");
    let r2 = call(
        office__sheet_select,
        &format!(r#"{{"path":"{path}","columns":[1],"output":"{iout}"}}"#),
    );
    assert_eq!(r2["columns"], 1, "one column: {r2}");
    let ri = call(office__sheet_read, &format!(r#"{{"path":"{iout}"}}"#));
    assert_eq!(
        ri["sheets"][0]["rows"][0][0], "qty",
        "index-selected header: {ri}"
    );
    assert_eq!(
        ri["sheets"][0]["rows"][1][0], 10.0,
        "index-selected value: {ri}"
    );

    for f in [&path, &out, &iout] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_join_inner_and_left() {
    let left = tmp("jl.xlsx");
    let right = tmp("jr.xlsx");
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{left}","sheets":[{{"name":"L","rows":[["id","name"],[1,"a"],[2,"b"],[3,"c"]]}}]}}"#
        ),
    );
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{right}","sheets":[{{"name":"R","rows":[["id","city"],[1,"NYC"],[2,"LA"]]}}]}}"#
        ),
    );

    // inner join on id -> only ids 1,2
    let out = tmp("ji2.xlsx");
    let r = call(
        office__sheet_join,
        &format!(r#"{{"left":"{left}","right":"{right}","on":"id","output":"{out}"}}"#),
    );
    assert_eq!(r["matched"], 2, "2 matches: {r}");
    assert_eq!(r["rows"], 2, "2 result rows: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = &rd["sheets"][0]["rows"];
    assert_eq!(rows[0][0], "id", "header id: {rd}");
    assert_eq!(rows[0][1], "name");
    assert_eq!(rows[0][2], "city", "right col joined: {rd}");
    assert_eq!(rows[1][1], "a", "id1 name: {rd}");
    assert_eq!(rows[1][2], "NYC", "id1 city: {rd}");

    // left join -> id 3 included with blank city
    let outl = tmp("jlj.xlsx");
    let rl = call(
        office__sheet_join,
        &format!(
            r#"{{"left":"{left}","right":"{right}","on":"id","how":"left","output":"{outl}"}}"#
        ),
    );
    assert_eq!(rl["rows"], 3, "left join keeps all 3: {rl}");
    let rdl = call(office__sheet_read, &format!(r#"{{"path":"{outl}"}}"#));
    let last = rdl["sheets"][0]["rows"].as_array().unwrap().last().unwrap();
    assert_eq!(last[1], "c", "id3 name: {rdl}");
    assert!(
        last[2].as_str().unwrap_or("").is_empty(),
        "id3 city blank: {rdl}"
    );

    // semi join: left rows that have a match -> ids 1,2 (left columns only)
    let outs = tmp("jsemi.xlsx");
    let rs = call(
        office__sheet_join,
        &format!(
            r#"{{"left":"{left}","right":"{right}","on":"id","how":"semi","output":"{outs}"}}"#
        ),
    );
    assert_eq!(rs["rows"], 2, "semi keeps 2: {rs}");
    let rds = call(office__sheet_read, &format!(r#"{{"path":"{outs}"}}"#));
    let srows = rds["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(
        srows[0].as_array().unwrap().len(),
        2,
        "semi: left columns only: {rds}"
    );

    // anti join: left rows with no match -> id 3
    let outa = tmp("janti.xlsx");
    let ra = call(
        office__sheet_join,
        &format!(
            r#"{{"left":"{left}","right":"{right}","on":"id","how":"anti","output":"{outa}"}}"#
        ),
    );
    assert_eq!(ra["rows"], 1, "anti keeps 1: {ra}");
    let rda = call(office__sheet_read, &format!(r#"{{"path":"{outa}"}}"#));
    assert_eq!(
        rda["sheets"][0]["rows"][1][0].as_f64().unwrap(),
        3.0,
        "anti -> id 3: {rda}"
    );

    for f in [&left, &right, &out, &outl, &outs, &outa] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_join_right_and_outer() {
    let left = tmp("rjl.xlsx");
    let right = tmp("rjr.xlsx");
    // left ids 1,2,3 ; right ids 1,2,4 (4 is right-only, 3 is left-only)
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{left}","sheets":[{{"name":"L","rows":[["id","name"],[1,"a"],[2,"b"],[3,"c"]]}}]}}"#
        ),
    );
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{right}","sheets":[{{"name":"R","rows":[["id","city"],[1,"NYC"],[2,"LA"],[4,"SF"]]}}]}}"#
        ),
    );

    // right join: ids 1,2 (matched) + right-only 4 = 3 rows; left-only 3 dropped
    let outr = tmp("rj_r.xlsx");
    let rr = call(
        office__sheet_join,
        &format!(
            r#"{{"left":"{left}","right":"{right}","on":"id","how":"right","output":"{outr}"}}"#
        ),
    );
    assert_eq!(rr["matched"], 2, "two matched pairs: {rr}");
    assert_eq!(rr["rows"], 3, "right join → 3 rows: {rr}");
    let rdr = call(office__sheet_read, &format!(r#"{{"path":"{outr}"}}"#));
    let lastr = rdr["sheets"][0]["rows"].as_array().unwrap().last().unwrap();
    assert_eq!(
        lastr[0].as_f64().unwrap(),
        4.0,
        "right-only key carried: {rdr}"
    );
    assert_eq!(lastr[2], "SF", "right-only city: {rdr}");
    assert!(
        lastr[1].as_str().unwrap_or("").is_empty(),
        "right-only name blank: {rdr}"
    );

    // outer join: all left (1,2,3) + right-only 4 = 4 rows
    let outo = tmp("rj_o.xlsx");
    let ro = call(
        office__sheet_join,
        &format!(
            r#"{{"left":"{left}","right":"{right}","on":"id","how":"outer","output":"{outo}"}}"#
        ),
    );
    assert_eq!(ro["rows"], 4, "outer join → 4 rows: {ro}");

    // unknown how is rejected
    let bad = call(
        office__sheet_join,
        &format!(
            r#"{{"left":"{left}","right":"{right}","on":"id","how":"cross","output":"{outo}"}}"#
        ),
    );
    assert!(bad["error"].is_string(), "unknown how rejected: {bad}");

    for f in [&left, &right, &outr, &outo] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_unpivot_melt() {
    let path = tmp("melt.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["id","jan","feb"],["A",1,2],["B",3,4]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("melt_out.xlsx");
    let r = call(
        office__sheet_unpivot,
        &format!(
            r#"{{"path":"{path}","id_vars":["id"],"value_vars":["jan","feb"],"output":"{out}"}}"#
        ),
    );
    assert_eq!(r["rows"], 4, "2 rows x 2 value cols = 4: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = &rd["sheets"][0]["rows"];
    assert_eq!(rows[0][0], "id", "id col kept: {rd}");
    assert_eq!(rows[0][1], "variable", "var col: {rd}");
    assert_eq!(rows[0][2], "value", "value col: {rd}");
    assert_eq!(rows[1][0], "A", "first melt id: {rd}");
    assert_eq!(rows[1][1], "jan", "first melt var: {rd}");
    assert_eq!(rows[1][2], 1.0, "first melt value: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_pivot_matrix() {
    let path = tmp("piv.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","quarter","amt"],
                ["west","Q1",10],
                ["west","Q2",20],
                ["east","Q1",5],
                ["east","Q1",15]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("piv_out.xlsx");
    let r = call(
        office__sheet_pivot,
        &format!(
            r#"{{"path":"{path}","rows":"region","cols":"quarter","value":"amt","agg":"sum","output":"{out}"}}"#
        ),
    );
    assert_eq!(r["rows"], 2, "2 row groups: {r}");
    assert_eq!(r["cols"], 2, "2 col groups: {r}");
    let rr = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = &rr["sheets"][0]["rows"];
    // header: [region, Q1, Q2]
    assert_eq!(rows[0][0], "region", "corner header: {rr}");
    assert_eq!(rows[0][1], "Q1");
    assert_eq!(rows[0][2], "Q2");
    // east: Q1=20 (5+15), Q2 missing -> 0
    assert_eq!(rows[1][0], "east", "first row group: {rr}");
    assert_eq!(rows[1][1], 20.0, "east Q1 sum: {rr}");
    assert_eq!(rows[1][2], 0.0, "east Q2 missing -> 0: {rr}");
    // west: Q1=10, Q2=20
    assert_eq!(rows[2][1], 10.0, "west Q1: {rr}");
    assert_eq!(rows[2][2], 20.0, "west Q2: {rr}");

    // with margins: a Total column + Total row + grand total
    let outm = tmp("piv_margins.xlsx");
    call(
        office__sheet_pivot,
        &format!(
            r#"{{"path":"{path}","rows":"region","cols":"quarter","value":"amt","agg":"sum","margins":true,"output":"{outm}"}}"#
        ),
    );
    let rm = call(office__sheet_read, &format!(r#"{{"path":"{outm}"}}"#));
    let mrows = rm["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(
        mrows[0].as_array().unwrap().last().unwrap(),
        "Total",
        "Total column header: {rm}"
    );
    // east row total = 20 (Q1 20 + Q2 0)
    assert_eq!(
        mrows[1]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .as_f64()
            .unwrap(),
        20.0,
        "east row total: {rm}"
    );
    // last row is the column-totals row; grand total = 50
    let last = mrows.last().unwrap().as_array().unwrap();
    assert_eq!(last[0], "Total", "totals row label: {rm}");
    assert_eq!(
        last.last().unwrap().as_f64().unwrap(),
        50.0,
        "grand total: {rm}"
    );

    for f in [&path, &out, &outm] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_aggregate_group_by() {
    let path = tmp("agg.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","amt"],
                ["west",10],
                ["east",5],
                ["west",20],
                ["east",15]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // sum amt by region -> east 20, west 30 (sorted by group key)
    let out = tmp("agg_sum.xlsx");
    let r = call(
        office__sheet_aggregate,
        &format!(
            r#"{{"path":"{path}","group_by":"region","value":"amt","agg":"sum","output":"{out}"}}"#
        ),
    );
    assert_eq!(r["groups"], 2, "two groups: {r}");
    let rr = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = &rr["sheets"][0]["rows"];
    assert_eq!(rows[0][1], "sum_amt", "agg label: {rr}");
    assert_eq!(rows[1][0], "east", "first group east: {rr}");
    assert_eq!(rows[1][1], 20.0, "east sum: {rr}");
    assert_eq!(rows[2][0], "west", "west: {rr}");
    assert_eq!(rows[2][1], 30.0, "west sum: {rr}");

    // count by region -> 2 each
    let cout = tmp("agg_count.xlsx");
    let c = call(
        office__sheet_aggregate,
        &format!(r#"{{"path":"{path}","group_by":"region","agg":"count","output":"{cout}"}}"#),
    );
    assert_eq!(c["groups"], 2, "count groups: {c}");
    let cr = call(office__sheet_read, &format!(r#"{{"path":"{cout}"}}"#));
    assert_eq!(cr["sheets"][0]["rows"][1][1], 2.0, "east count 2: {cr}");

    for f in [&path, &out, &cout] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_resample_by_month() {
    let path = tmp("resample.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["date","amt"],
                ["2026-01-05",10],
                ["2026-01-20",20],
                ["2026-02-02",5]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // monthly sum -> 2026-01: 30, 2026-02: 5
    let out = tmp("resample_out.xlsx");
    let r = call(
        office__sheet_resample,
        &format!(
            r#"{{"path":"{path}","date":"date","value":"amt","freq":"month","output":"{out}"}}"#
        ),
    );
    assert_eq!(r["buckets"], 2, "two month buckets: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][0], "2026-01", "first bucket key: {rd}");
    assert_eq!(rows[1][1].as_f64().unwrap(), 30.0, "Jan sum: {rd}");
    assert_eq!(rows[2][0], "2026-02", "second bucket: {rd}");
    assert_eq!(rows[2][1].as_f64().unwrap(), 5.0, "Feb sum: {rd}");

    // yearly -> one bucket
    let outy = tmp("resample_y.xlsx");
    let ry = call(
        office__sheet_resample,
        &format!(
            r#"{{"path":"{path}","date":"date","value":"amt","freq":"year","output":"{outy}"}}"#
        ),
    );
    assert_eq!(ry["buckets"], 1, "one year bucket: {ry}");
    let rdy = call(office__sheet_read, &format!(r#"{{"path":"{outy}"}}"#));
    assert_eq!(
        rdy["sheets"][0]["rows"][1][0], "2026",
        "year bucket key: {rdy}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outy).ok();
}

#[test]
fn sheet_date_part_extract() {
    let path = tmp("datepart.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["d"],["2026-06-14"],["2025-12-01"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // month into a new column
    let out = tmp("datepart_out.xlsx");
    let r = call(
        office__sheet_date_part,
        &format!(r#"{{"path":"{path}","output":"{out}","column":"d","part":"month","into":"mo"}}"#),
    );
    assert_eq!(r["column"], "mo", "new column: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][1], "mo", "header appended: {rd}");
    assert_eq!(rows[1][1].as_f64().unwrap(), 6.0, "June -> 6: {rd}");
    assert_eq!(rows[2][1].as_f64().unwrap(), 12.0, "December -> 12: {rd}");

    // ym part is text YYYY-MM
    let outy = tmp("datepart_ym.xlsx");
    call(
        office__sheet_date_part,
        &format!(r#"{{"path":"{path}","output":"{outy}","column":"d","part":"ym"}}"#),
    );
    let rdy = call(office__sheet_read, &format!(r#"{{"path":"{outy}"}}"#));
    assert_eq!(rdy["sheets"][0]["rows"][1][1], "2026-06", "ym slice: {rdy}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outy).ok();
}

#[test]
fn sheet_date_diff_days_between() {
    let path = tmp("datediff.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["start","end"],
                ["2026-01-01","2026-01-08"],
                ["2024-02-28","2024-03-01"],
                ["2026-06-14","2026-06-14"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("datediff_out.xlsx");
    let r = call(
        office__sheet_date_diff,
        &format!(
            r#"{{"path":"{path}","start":"start","end":"end","output":"{out}","into":"gap"}}"#
        ),
    );
    assert_eq!(r["column"], "gap", "new column: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][2], "gap", "header appended: {rd}");
    assert_eq!(rows[1][2].as_f64().unwrap(), 7.0, "one week = 7 days: {rd}");
    // 2024 is a leap year: Feb 28 -> Mar 1 is 2 days
    assert_eq!(rows[2][2].as_f64().unwrap(), 2.0, "leap-year span: {rd}");
    assert_eq!(rows[3][2].as_f64().unwrap(), 0.0, "same date = 0: {rd}");

    // weeks unit
    let outw = tmp("datediff_wk.xlsx");
    call(
        office__sheet_date_diff,
        &format!(
            r#"{{"path":"{path}","start":"start","end":"end","output":"{outw}","unit":"weeks","decimals":3}}"#
        ),
    );
    let rdw = call(office__sheet_read, &format!(r#"{{"path":"{outw}"}}"#));
    assert_eq!(
        rdw["sheets"][0]["rows"][1][2].as_f64().unwrap(),
        1.0,
        "7 days = 1 week: {rdw}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outw).ok();
}

#[test]
fn sheet_networkdays_business_days() {
    let path = tmp("netdays.xlsx");
    // 2026-06-15 (Mon) .. 2026-06-19 (Fri) inclusive = 5 business days
    // 2026-06-15 (Mon) .. 2026-06-22 (next Mon) = 6 (skips Sat 20, Sun 21)
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["s","e"],
                ["2026-06-15","2026-06-19"],
                ["2026-06-15","2026-06-22"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("netdays_out.xlsx");
    let r = call(
        office__sheet_networkdays,
        &format!(r#"{{"path":"{path}","start":"s","end":"e","output":"{out}","into":"bd"}}"#),
    );
    assert_eq!(r["column"], "bd", "new column: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(
        rows[1][2].as_f64().unwrap(),
        5.0,
        "Mon-Fri inclusive = 5: {rd}"
    );
    assert_eq!(
        rows[2][2].as_f64().unwrap(),
        6.0,
        "spanning a weekend = 6: {rd}"
    );

    // with a holiday inside the first range -> 4
    let outh = tmp("netdays_hol.xlsx");
    call(
        office__sheet_networkdays,
        &format!(
            r#"{{"path":"{path}","start":"s","end":"e","output":"{outh}","holidays":["2026-06-17"]}}"#
        ),
    );
    let rdh = call(office__sheet_read, &format!(r#"{{"path":"{outh}"}}"#));
    assert_eq!(
        rdh["sheets"][0]["rows"][1][2].as_f64().unwrap(),
        4.0,
        "holiday excluded: {rdh}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outh).ok();
}

#[test]
fn sheet_date_add_shift() {
    let path = tmp("dateadd.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["d"],
                ["2026-01-31"],
                ["2026-12-30"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // +5 days: Jan 31 -> Feb 5; Dec 30 -> Jan 4 next year (rollover)
    let out = tmp("dateadd_days.xlsx");
    let r = call(
        office__sheet_date_add,
        &format!(r#"{{"path":"{path}","column":"d","amount":5,"output":"{out}","into":"plus"}}"#),
    );
    assert_eq!(r["column"], "plus", "new column: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][1], "2026-02-05", "Jan31 +5d: {rd}");
    assert_eq!(rows[2][1], "2027-01-04", "Dec30 +5d rolls year: {rd}");

    // +1 month: Jan 31 clamps to Feb 28 (2026 not leap)
    let outm = tmp("dateadd_mon.xlsx");
    call(
        office__sheet_date_add,
        &format!(
            r#"{{"path":"{path}","column":"d","amount":1,"unit":"months","output":"{outm}"}}"#
        ),
    );
    let rdm = call(office__sheet_read, &format!(r#"{{"path":"{outm}"}}"#));
    assert_eq!(
        rdm["sheets"][0]["rows"][1][1], "2026-02-28",
        "Jan31 +1mo clamps to Feb28: {rdm}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outm).ok();
}

#[test]
fn sheet_weekday_names() {
    let path = tmp("weekday.xlsx");
    // 2026-06-14 is a Sunday; 2026-06-15 a Monday
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["d"],["2026-06-14"],["2026-06-15"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("weekday_out.xlsx");
    let r = call(
        office__sheet_weekday,
        &format!(r#"{{"path":"{path}","column":"d","output":"{out}","into":"dow"}}"#),
    );
    assert_eq!(r["column"], "dow", "new column: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][1], "Sunday", "2026-06-14 is Sunday: {rd}");
    assert_eq!(rows[2][1], "Monday", "2026-06-15 is Monday: {rd}");

    // ISO number: Sunday=7, Monday=1
    let outi = tmp("weekday_iso.xlsx");
    call(
        office__sheet_weekday,
        &format!(r#"{{"path":"{path}","column":"d","output":"{outi}","format":"iso"}}"#),
    );
    let rdi = call(office__sheet_read, &format!(r#"{{"path":"{outi}"}}"#));
    assert_eq!(
        rdi["sheets"][0]["rows"][1][1].as_f64().unwrap(),
        7.0,
        "Sunday ISO 7: {rdi}"
    );
    assert_eq!(
        rdi["sheets"][0]["rows"][2][1].as_f64().unwrap(),
        1.0,
        "Monday ISO 1: {rdi}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outi).ok();
}

#[test]
fn sheet_group_stats_per_group() {
    let path = tmp("grpstats.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[["g","v"],["a",2],["a",4],["a",6],["b",10],["b",20]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("grpstats_out.xlsx");
    let r = call(
        office__sheet_group_stats,
        &format!(r#"{{"path":"{path}","output":"{out}","group":"g","value":"v"}}"#),
    );
    assert_eq!(r["ok"], true, "group_stats: {r}");
    assert_eq!(r["groups"].as_u64().unwrap(), 2, "two groups: {r}");

    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    // header: [g, count, mean, std, min, max]
    assert_eq!(rows[0][0], "g", "group header keeps name: {rd}");
    assert_eq!(rows[0][1], "count", "count header: {rd}");
    // group a: 2,4,6 -> count 3, mean 4, min 2, max 6, sample std 2
    assert_eq!(rows[1][0], "a", "first group sorted: {rd}");
    assert_eq!(rows[1][1].as_f64().unwrap(), 3.0, "a count: {rd}");
    assert_eq!(rows[1][2].as_f64().unwrap(), 4.0, "a mean: {rd}");
    assert!(
        (rows[1][3].as_f64().unwrap() - 2.0).abs() < 1e-9,
        "a std: {rd}"
    );
    assert_eq!(rows[1][4].as_f64().unwrap(), 2.0, "a min: {rd}");
    assert_eq!(rows[1][5].as_f64().unwrap(), 6.0, "a max: {rd}");
    // group b: 10,20 -> count 2, mean 15, sample std ~7.0710678
    assert_eq!(rows[2][0], "b", "second group: {rd}");
    assert_eq!(rows[2][2].as_f64().unwrap(), 15.0, "b mean: {rd}");
    assert!(
        (rows[2][3].as_f64().unwrap() - 7.071_067_811_865_476).abs() < 1e-9,
        "b std: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_freq_value_counts() {
    let path = tmp("freq.xlsx");
    // west x3, east x2, north x1; one blank (skipped)
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region"],
                ["west"],
                ["east"],
                ["west"],
                ["north"],
                ["east"],
                ["west"],
                [""]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_freq,
        &format!(r#"{{"path":"{path}","column":"region"}}"#),
    );
    assert_eq!(r["column"], "region", "column name: {r}");
    assert_eq!(r["total"], 6, "blank skipped, 6 counted: {r}");
    assert_eq!(r["distinct"], 3, "three distinct: {r}");
    let v = r["values"].as_array().unwrap();
    // sorted by count desc: west(3), east(2), north(1)
    assert_eq!(v[0]["value"], "west", "most frequent first: {r}");
    assert_eq!(v[0]["count"], 3, "west count: {r}");
    assert_eq!(v[1]["value"], "east", "second: {r}");
    assert_eq!(v[2]["value"], "north", "least: {r}");
    assert!(
        (v[0]["pct"].as_f64().unwrap() - 50.0).abs() < 1e-9,
        "west 50%: {r}"
    );

    // top=1 keeps only the most frequent
    let t = call(
        office__sheet_freq,
        &format!(r#"{{"path":"{path}","column":"region","top":1}}"#),
    );
    assert_eq!(t["values"].as_array().unwrap().len(), 1, "top limited: {t}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_unique_distinct_values() {
    let path = tmp("unique.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["c"],["b"],["a"],["b"],["c"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // first-seen order: b, a, c
    let r = call(
        office__sheet_unique,
        &format!(r#"{{"path":"{path}","column":"c"}}"#),
    );
    assert_eq!(r["count"], 3, "three distinct: {r}");
    assert_eq!(r["values"][0], "b", "first-seen b: {r}");
    assert_eq!(r["values"][1], "a", "then a: {r}");
    assert_eq!(r["values"][2], "c", "then c: {r}");

    // sorted: a, b, c
    let s = call(
        office__sheet_unique,
        &format!(r#"{{"path":"{path}","column":"c","sorted":true}}"#),
    );
    assert_eq!(s["values"][0], "a", "sorted first a: {s}");
    assert_eq!(s["values"][2], "c", "sorted last c: {s}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_split_column_text_to_columns() {
    let path = tmp("splitcol.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["name","age"],
                ["Doe, John",30],
                ["Smith, Jane",25]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // Split "name" by ", " into last/first, replacing the original column.
    let out = tmp("splitcol_out.xlsx");
    let r = call(
        office__sheet_split_column,
        &serde_json::json!({
            "path": path, "column": "name", "delimiter": ", ",
            "into": ["last", "first"], "output": out
        })
        .to_string(),
    );
    assert_eq!(r["columns"], 2, "two new columns: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    // header: last, first, age (original "name" replaced, trailing "age" kept)
    assert_eq!(rows[0][0], "last", "first new header: {rd}");
    assert_eq!(rows[0][1], "first", "second new header: {rd}");
    assert_eq!(rows[0][2], "age", "trailing column preserved: {rd}");
    assert_eq!(rows[1][0], "Doe", "row1 last: {rd}");
    assert_eq!(rows[1][1], "John", "row1 first (trimmed): {rd}");
    assert_eq!(rows[1][2].as_f64().unwrap(), 30.0, "row1 age intact: {rd}");
    assert_eq!(rows[2][0], "Smith", "row2 last: {rd}");

    // Auto-named, uneven widths padded with blanks.
    let p2 = tmp("splitcol2.xlsx");
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{p2}","sheets":[{{"name":"D","rows":[["tag"],["a-b-c"],["x-y"]]}}]}}"#
        ),
    );
    let o2 = tmp("splitcol2_out.xlsx");
    let r2 = call(
        office__sheet_split_column,
        &serde_json::json!({ "path": p2, "column": "tag", "delimiter": "-", "output": o2 })
            .to_string(),
    );
    assert_eq!(r2["columns"], 3, "widest split = 3: {r2}");
    let rd2 = call(office__sheet_read, &format!(r#"{{"path":"{o2}"}}"#));
    let rows2 = rd2["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows2[0][0], "tag_1", "auto header: {rd2}");
    assert_eq!(rows2[2][1], "y", "short row part: {rd2}");
    // padded blank round-trips through xlsx as an empty/null cell
    assert_eq!(
        rows2[2][2].as_str().unwrap_or(""),
        "",
        "short row padded blank: {rd2}"
    );

    for f in [&path, &out, &p2, &o2] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_concat_columns_textjoin() {
    let path = tmp("concatcol.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["first","last","age"],
                ["John","Doe",30],
                ["Jane","Smith",25]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // Join first+last into "name" with a space, replacing the source columns.
    let out = tmp("concatcol_out.xlsx");
    let r = call(
        office__sheet_concat_columns,
        &serde_json::json!({
            "path": path, "columns": ["first", "last"], "separator": " ",
            "into": "name", "output": out
        })
        .to_string(),
    );
    assert_eq!(r["into"], "name", "merged header: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    // merged placed at leftmost source index; "age" preserved
    assert_eq!(rows[0][0], "name", "merged header at col 0: {rd}");
    assert_eq!(rows[0][1], "age", "trailing column preserved: {rd}");
    assert_eq!(rows[1][0], "John Doe", "row1 joined: {rd}");
    assert_eq!(rows[1][1].as_f64().unwrap(), 30.0, "row1 age intact: {rd}");
    assert_eq!(rows[2][0], "Jane Smith", "row2 joined: {rd}");

    // keep=true appends the merged column at the end, leaving originals intact.
    let outk = tmp("concatcol_keep.xlsx");
    call(
        office__sheet_concat_columns,
        &serde_json::json!({
            "path": path, "columns": ["last", "first"], "separator": ", ",
            "into": "full", "keep": true, "output": outk
        })
        .to_string(),
    );
    let rk = call(office__sheet_read, &format!(r#"{{"path":"{outk}"}}"#));
    let rowsk = rk["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rowsk[0][0], "first", "original col 0 kept: {rk}");
    assert_eq!(rowsk[0][3], "full", "merged appended at end: {rk}");
    assert_eq!(
        rowsk[1][3], "Doe, John",
        "join uses user order last,first: {rk}"
    );

    for f in [&path, &out, &outk] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_str_distance_fuzzy() {
    let path = tmp("strdist.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["a","b"],["kitten","sitting"],["abc","abc"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // Levenshtein: kitten->sitting = 3; abc==abc = 0
    let out = tmp("strdist_out.xlsx");
    let r = call(
        office__sheet_str_distance,
        &format!(r#"{{"path":"{path}","a":"a","b":"b","output":"{out}","into":"d"}}"#),
    );
    assert_eq!(r["column"], "d", "new column: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(
        rows[1][2].as_f64().unwrap(),
        3.0,
        "kitten/sitting = 3: {rd}"
    );
    assert_eq!(rows[2][2].as_f64().unwrap(), 0.0, "identical = 0: {rd}");

    // ratio metric: identical -> 1.0; kitten/sitting -> 1 - 3/7 ≈ 0.5714
    let outr = tmp("strdist_ratio.xlsx");
    call(
        office__sheet_str_distance,
        &format!(
            r#"{{"path":"{path}","a":"a","b":"b","output":"{outr}","metric":"ratio","decimals":4}}"#
        ),
    );
    let rdr = call(office__sheet_read, &format!(r#"{{"path":"{outr}"}}"#));
    assert_eq!(
        rdr["sheets"][0]["rows"][2][2].as_f64().unwrap(),
        1.0,
        "identical ratio 1: {rdr}"
    );
    assert!(
        (rdr["sheets"][0]["rows"][1][2].as_f64().unwrap() - 0.5714).abs() < 1e-3,
        "ratio 1-3/7: {rdr}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outr).ok();
}

#[test]
fn sheet_group_concat_joins_values() {
    let path = tmp("gconcat.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","name"],
                ["west","a"],
                ["east","b"],
                ["west","c"],
                ["west","a"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // group by region, concat names (sorted by key: east, west)
    let out = tmp("gconcat_out.xlsx");
    let r = call(
        office__sheet_group_concat,
        &format!(
            r#"{{"path":"{path}","group_by":"region","value":"name","output":"{out}","sep":", "}}"#
        ),
    );
    assert_eq!(r["groups"], 2, "two groups: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "region", "group header: {rd}");
    assert_eq!(rows[0][1], "name_list", "value header: {rd}");
    assert_eq!(rows[1][0], "east", "first group key: {rd}");
    assert_eq!(rows[1][1], "b", "east values: {rd}");
    assert_eq!(rows[2][0], "west", "second group key: {rd}");
    assert_eq!(rows[2][1], "a, c, a", "west values (dupes kept): {rd}");

    // distinct dedupes within a group
    let outd = tmp("gconcat_d.xlsx");
    call(
        office__sheet_group_concat,
        &format!(
            r#"{{"path":"{path}","group_by":"region","value":"name","output":"{outd}","distinct":true}}"#
        ),
    );
    let rdd = call(office__sheet_read, &format!(r#"{{"path":"{outd}"}}"#));
    assert_eq!(
        rdd["sheets"][0]["rows"][2][1], "a, c",
        "west distinct: {rdd}"
    );

    for f in [&path, &out, &outd] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_lookup_vlookup() {
    let path = tmp("lookup.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","price"],["apple",10],["banana",20]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // found
    let r = call(
        office__sheet_lookup,
        &format!(r#"{{"path":"{path}","lookup":"name","key":"banana","result":"price"}}"#),
    );
    assert_eq!(r["found"], true, "found: {r}");
    assert_eq!(r["value"].as_f64().unwrap(), 20.0, "returned price: {r}");
    assert_eq!(r["row"], 1, "0-based data row: {r}");

    // case-insensitive match
    let ci = call(
        office__sheet_lookup,
        &format!(
            r#"{{"path":"{path}","lookup":"name","key":"APPLE","result":"price","ignore_case":true}}"#
        ),
    );
    assert_eq!(ci["value"].as_f64().unwrap(), 10.0, "ci lookup: {ci}");

    // not found
    let nf = call(
        office__sheet_lookup,
        &format!(r#"{{"path":"{path}","lookup":"name","key":"cherry","result":"price"}}"#),
    );
    assert_eq!(nf["found"], false, "not found: {nf}");
    assert_eq!(nf["row"], -1, "row -1: {nf}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_countif_sumif() {
    let path = tmp("countif.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","qty"],
                ["west",10],
                ["east",20],
                ["west",30]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // countif region == west -> 2
    let c = call(
        office__sheet_countif,
        &format!(r#"{{"path":"{path}","column":"region","op":"eq","value":"west"}}"#),
    );
    assert_eq!(c["count"], 2, "two west rows: {c}");

    // countif qty >= 20 -> 2
    let cn = call(
        office__sheet_countif,
        &format!(r#"{{"path":"{path}","column":"qty","op":"ge","value":20}}"#),
    );
    assert_eq!(cn["count"], 2, "two rows qty>=20: {cn}");

    // sumif qty where region == west -> 40
    let s = call(
        office__sheet_sumif,
        &format!(r#"{{"path":"{path}","column":"region","op":"eq","value":"west","sum":"qty"}}"#),
    );
    assert_eq!(s["count"], 2, "two matched: {s}");
    assert_eq!(s["sum"].as_f64().unwrap(), 40.0, "west qty sum: {s}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_sumifs_multi_criteria() {
    let path = tmp("sumifs.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","product","qty"],
                ["west","A",10],
                ["west","B",20],
                ["east","A",30],
                ["west","A",5]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // region==west AND product==A -> rows (west,A,10) + (west,A,5) = 15
    let s = call(
        office__sheet_sumifs,
        &format!(
            r#"{{"path":"{path}","conditions":[{{"column":"region","value":"west"}},{{"column":"product","value":"A"}}],"sum":"qty"}}"#
        ),
    );
    assert_eq!(s["count"].as_u64().unwrap(), 2, "two rows match both: {s}");
    assert_eq!(s["sum"].as_f64().unwrap(), 15.0, "sumifs west+A qty: {s}");

    // match any: region==east OR product==B -> east,A,30 + west,B,20 = 50 over 2 rows
    let any = call(
        office__sheet_sumifs,
        &format!(
            r#"{{"path":"{path}","conditions":[{{"column":"region","value":"east"}},{{"column":"product","value":"B"}}],"sum":"qty","match":"any"}}"#
        ),
    );
    assert_eq!(
        any["count"].as_u64().unwrap(),
        2,
        "two rows match any: {any}"
    );
    assert_eq!(any["sum"].as_f64().unwrap(), 50.0, "sumifs any: {any}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_filter_rows() {
    let path = tmp("filt.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","qty"],["a",10],["b",20],["c",30]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // numeric: qty >= 20 keeps b,c
    let ge = tmp("filt_ge.xlsx");
    let r = call(
        office__sheet_filter,
        &format!(r#"{{"path":"{path}","by":"qty","op":"ge","value":20,"output":"{ge}"}}"#),
    );
    assert_eq!(r["kept"], 2, "kept 2: {r}");
    assert_eq!(r["removed"], 1, "removed 1: {r}");
    let rg = call(office__sheet_read, &format!(r#"{{"path":"{ge}"}}"#));
    let rows = rg["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "header + 2 rows: {rg}");
    assert_eq!(rows[1][0], "b", "first kept is b: {rg}");

    // string eq: name == "a" keeps 1
    let eq = tmp("filt_eq.xlsx");
    let r2 = call(
        office__sheet_filter,
        &format!(r#"{{"path":"{path}","by":"name","op":"eq","value":"a","output":"{eq}"}}"#),
    );
    assert_eq!(r2["kept"], 1, "eq kept 1: {r2}");

    for f in [&path, &ge, &eq] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_flag_marks_rows() {
    let path = tmp("flag.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","qty"],["a",10],["b",20],["c",30]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // flag qty >= 20 with default 1/0 → all rows kept, b & c flagged
    let out = tmp("flag_out.xlsx");
    let r = call(
        office__sheet_flag,
        &format!(
            r#"{{"path":"{path}","by":"qty","op":"ge","value":20,"into":"big","output":"{out}"}}"#
        ),
    );
    assert_eq!(r["column"], "big", "flag column name: {r}");
    assert_eq!(r["flagged"], 2, "two rows flagged: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4, "all rows kept (header + 3): {rd}");
    assert_eq!(rows[0][2], "big", "header appended: {rd}");
    assert_eq!(rows[1][2].as_f64().unwrap(), 0.0, "a not flagged: {rd}");
    assert_eq!(rows[2][2].as_f64().unwrap(), 1.0, "b flagged: {rd}");
    assert_eq!(rows[3][2].as_f64().unwrap(), 1.0, "c flagged: {rd}");

    // custom labels
    let outl = tmp("flag_lbl.xlsx");
    call(
        office__sheet_flag,
        &serde_json::json!({
            "path": path, "by": "name", "op": "eq", "value": "a",
            "true_value": "Y", "false_value": "N", "into": "isA", "output": outl
        })
        .to_string(),
    );
    let rdl = call(office__sheet_read, &format!(r#"{{"path":"{outl}"}}"#));
    let rowsl = rdl["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rowsl[1][2], "Y", "a -> Y: {rdl}");
    assert_eq!(rowsl[2][2], "N", "b -> N: {rdl}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outl).ok();
}

#[test]
fn sheet_onehot_encode() {
    let path = tmp("onehot.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["color","n"],["red",1],["blue",2],["red",3]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("onehot_out.xlsx");
    let r = call(
        office__sheet_onehot,
        &format!(r#"{{"path":"{path}","output":"{out}","column":"color"}}"#),
    );
    let cats: Vec<String> = r["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(cats, vec!["red", "blue"], "first-seen category order: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    // original 2 cols + color_red, color_blue appended
    assert_eq!(rows[0][2], "color_red", "indicator header: {rd}");
    assert_eq!(rows[0][3], "color_blue", "indicator header: {rd}");
    assert_eq!(rows[1][2].as_f64().unwrap(), 1.0, "row1 red=1: {rd}");
    assert_eq!(rows[1][3].as_f64().unwrap(), 0.0, "row1 blue=0: {rd}");
    assert_eq!(rows[2][2].as_f64().unwrap(), 0.0, "row2 red=0: {rd}");
    assert_eq!(rows[2][3].as_f64().unwrap(), 1.0, "row2 blue=1: {rd}");

    // drop removes the original column
    let outd = tmp("onehot_drop.xlsx");
    call(
        office__sheet_onehot,
        &format!(r#"{{"path":"{path}","output":"{outd}","column":"color","drop":true}}"#),
    );
    let rdd = call(office__sheet_read, &format!(r#"{{"path":"{outd}"}}"#));
    let rowsd = rdd["sheets"][0]["rows"].as_array().unwrap();
    // after drop: n, color_red, color_blue
    assert_eq!(rowsd[0][0], "n", "original color column dropped: {rdd}");
    assert_eq!(rowsd[0][1], "color_red", "indicator after drop: {rdd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outd).ok();
}

#[test]
fn sheet_sort_by_column() {
    let path = tmp("sort.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","qty"],["c",30],["a",10],["b",20]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // numeric ascending by qty -> 10,20,30 => names a,b,c
    let asc = tmp("sort_asc.xlsx");
    let r = call(
        office__sheet_sort,
        &format!(r#"{{"path":"{path}","by":"qty","output":"{asc}"}}"#),
    );
    assert_eq!(r["sorted"], 3, "sorted 3 rows: {r}");
    let ra = call(office__sheet_read, &format!(r#"{{"path":"{asc}"}}"#));
    let rows = &ra["sheets"][0]["rows"];
    assert_eq!(rows[0][0], "name", "header preserved: {ra}");
    assert_eq!(rows[1][0], "a", "smallest qty first: {ra}");
    assert_eq!(rows[3][0], "c", "largest qty last: {ra}");

    // text descending by name -> c,b,a
    let desc = tmp("sort_desc.xlsx");
    let r2 = call(
        office__sheet_sort,
        &format!(r#"{{"path":"{path}","by":"name","output":"{desc}","descending":true}}"#),
    );
    assert_eq!(r2["ok"], true, "desc sort: {r2}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{desc}"}}"#));
    assert_eq!(rd["sheets"][0]["rows"][1][0], "c", "desc name first: {rd}");

    for f in [&path, &asc, &desc] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_json_interop_round_trip() {
    let path = tmp("ji.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","qty"],["a",1],["b",2]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // sheet -> json file
    let jf = tmp("ji.json");
    let sj = call(
        office__sheet_to_json,
        &format!(r#"{{"path":"{path}","output":"{jf}"}}"#),
    );
    assert_eq!(sj["count"], 2, "two records exported: {sj}");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&jf).unwrap()).unwrap();
    assert_eq!(parsed[0]["name"], "a", "json rec0 name: {parsed}");
    assert_eq!(parsed[1]["qty"], 2.0, "json rec1 qty: {parsed}");

    // json file -> sheet
    let out = tmp("ji_out.xlsx");
    let js = call(
        office__json_to_sheet,
        &format!(r#"{{"input":"{jf}","output":"{out}"}}"#),
    );
    assert_eq!(js["rows"], 2, "wrote 2 rows: {js}");
    let r = call(office__sheet_records, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(r["records"][0]["name"], "a", "round-trip name: {r}");
    assert_eq!(r["records"][1]["qty"], 2.0, "round-trip qty: {r}");

    for f in [&path, &jf, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_drop_empty_rows_and_cols() {
    let path = tmp("dropempty.xlsx");
    // column "y" all-blank in data; one fully-blank row
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["x","y"],
                [1,""],
                ["",""],
                [2,""]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("dropempty_out.xlsx");
    let r = call(
        office__sheet_drop_empty,
        &format!(r#"{{"path":"{path}","output":"{out}","rows":true,"cols":true}}"#),
    );
    assert_eq!(r["rows_removed"], 1, "one blank row removed: {r}");
    assert_eq!(r["cols_removed"], 1, "one blank column removed: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "header + 2 data rows: {rd}");
    assert_eq!(
        rows[0].as_array().unwrap().len(),
        1,
        "only column x kept: {rd}"
    );
    assert_eq!(rows[0][0], "x", "header x: {rd}");
    assert_eq!(rows[1][0].as_f64().unwrap(), 1.0, "first data: {rd}");
    assert_eq!(
        rows[2][0].as_f64().unwrap(),
        2.0,
        "blank row gone, 2 remains: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_dropna_subset() {
    let path = tmp("dropna.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["id","email"],
                [1,"a@x.com"],
                [2,""],
                [3,"c@x.com"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // drop rows missing "email"
    let out = tmp("dropna_out.xlsx");
    let r = call(
        office__sheet_dropna,
        &format!(r#"{{"path":"{path}","by":"email","output":"{out}"}}"#),
    );
    assert_eq!(r["kept"], 2, "two rows with email kept: {r}");
    assert_eq!(r["removed"], 1, "one removed: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "header + 2 rows: {rd}");
    assert_eq!(rows[1][0].as_f64().unwrap(), 1.0, "row 1 kept: {rd}");
    assert_eq!(
        rows[2][0].as_f64().unwrap(),
        3.0,
        "row 3 kept (row 2 dropped): {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_add_header_prepends() {
    let path = tmp("addhdr.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[[1,2],[3,4]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("addhdr_out.xlsx");
    let r = call(
        office__sheet_add_header,
        &serde_json::json!({ "path": path, "names": ["a", "b"], "output": out }).to_string(),
    );
    assert_eq!(r["columns"], 2, "two header columns: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "header + 2 data rows: {rd}");
    assert_eq!(rows[0][0], "a", "header a: {rd}");
    assert_eq!(rows[0][1], "b", "header b: {rd}");
    assert_eq!(
        rows[1][0].as_f64().unwrap(),
        1.0,
        "old first row now data: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_add_index_numbers_rows() {
    let path = tmp("addidx.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name"],["a"],["b"],["c"]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("addidx_out.xlsx");
    let r = call(
        office__sheet_add_index,
        &format!(r#"{{"path":"{path}","output":"{out}","name":"id"}}"#),
    );
    assert_eq!(r["rows"], 3, "three rows numbered: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "id", "index header prepended: {rd}");
    assert_eq!(rows[0][1], "name", "original header shifted right: {rd}");
    assert_eq!(rows[1][0].as_f64().unwrap(), 1.0, "first index 1: {rd}");
    assert_eq!(rows[3][0].as_f64().unwrap(), 3.0, "third index 3: {rd}");
    assert_eq!(rows[1][1], "a", "original data preserved: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_calc_arithmetic_column() {
    let path = tmp("calc.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["qty","price"],[2,3],[4,5]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // total = qty * price
    let out = tmp("calc_out.xlsx");
    let r = call(
        office__sheet_calc,
        &format!(
            r#"{{"path":"{path}","into":"total","left":"qty","op":"*","right":"price","output":"{out}"}}"#
        ),
    );
    assert_eq!(r["column"], "total", "new column: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][2], "total", "header appended: {rd}");
    assert_eq!(rows[1][2].as_f64().unwrap(), 6.0, "2*3=6: {rd}");
    assert_eq!(rows[2][2].as_f64().unwrap(), 20.0, "4*5=20: {rd}");

    // column op constant: qty * 10
    let outc = tmp("calc_c.xlsx");
    call(
        office__sheet_calc,
        &format!(
            r#"{{"path":"{path}","into":"x10","left":"qty","op":"*","value":10,"output":"{outc}"}}"#
        ),
    );
    let rdc = call(office__sheet_read, &format!(r#"{{"path":"{outc}"}}"#));
    assert_eq!(
        rdc["sheets"][0]["rows"][1][2].as_f64().unwrap(),
        20.0,
        "2*10=20: {rdc}"
    );

    for f in [&path, &out, &outc] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_row_stats_horizontal() {
    let path = tmp("rowstats.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[["q1","q2","q3"],[1,2,3],[10,20,30]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // sum across the three columns
    let out = tmp("rowstats_sum.xlsx");
    let r = call(
        office__sheet_row_stats,
        &format!(
            r#"{{"path":"{path}","output":"{out}","columns":["q1","q2","q3"],"agg":"sum","into":"total"}}"#
        ),
    );
    assert_eq!(r["column"], "total", "new column: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][3], "total", "header appended: {rd}");
    assert_eq!(rows[1][3].as_f64().unwrap(), 6.0, "1+2+3=6: {rd}");
    assert_eq!(rows[2][3].as_f64().unwrap(), 60.0, "10+20+30=60: {rd}");

    // mean across columns, default column name row_mean
    let outm = tmp("rowstats_mean.xlsx");
    let rm = call(
        office__sheet_row_stats,
        &format!(
            r#"{{"path":"{path}","output":"{outm}","columns":["q1","q2","q3"],"agg":"mean"}}"#
        ),
    );
    assert_eq!(rm["column"], "row_mean", "default name: {rm}");
    let rdm = call(office__sheet_read, &format!(r#"{{"path":"{outm}"}}"#));
    assert_eq!(
        rdm["sheets"][0]["rows"][2][3].as_f64().unwrap(),
        20.0,
        "mean 10,20,30 = 20: {rdm}"
    );

    // range across columns = max - min
    let outr = tmp("rowstats_range.xlsx");
    call(
        office__sheet_row_stats,
        &format!(
            r#"{{"path":"{path}","output":"{outr}","columns":["q1","q2","q3"],"agg":"range"}}"#
        ),
    );
    let rdr = call(office__sheet_read, &format!(r#"{{"path":"{outr}"}}"#));
    assert_eq!(
        rdr["sheets"][0]["rows"][2][3].as_f64().unwrap(),
        20.0,
        "range 10..30 = 20: {rdr}"
    );

    for f in [&path, &out, &outm, &outr] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_where_multi_condition() {
    let path = tmp("where.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["region","qty"],
                ["west",10],
                ["east",20],
                ["west",30]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // AND: region == west AND qty >= 20  -> only west/30
    let out = tmp("where_out.xlsx");
    let r = call(
        office__sheet_where,
        &serde_json::json!({
            "path": path, "output": out,
            "conditions": [
                {"column": "region", "op": "eq", "value": "west"},
                {"column": "qty", "op": "ge", "value": 20}
            ]
        })
        .to_string(),
    );
    assert_eq!(r["kept"], 1, "one row matches both: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "header + 1 row: {rd}");
    assert_eq!(rows[1][1].as_f64().unwrap(), 30.0, "the west/30 row: {rd}");

    // OR: region == east OR qty >= 30 -> east/20 and west/30
    let outo = tmp("where_or.xlsx");
    let ro = call(
        office__sheet_where,
        &serde_json::json!({
            "path": path, "output": outo, "match": "any",
            "conditions": [
                {"column": "region", "op": "eq", "value": "east"},
                {"column": "qty", "op": "ge", "value": 30}
            ]
        })
        .to_string(),
    );
    assert_eq!(ro["kept"], 2, "two rows match either: {ro}");

    for f in [&path, &out, &outo] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_freeze_panes() {
    use std::io::Read as _;
    let path = tmp("freeze.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["h"],[1],[2]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("freeze_out.xlsx");
    let r = call(
        office__sheet_freeze,
        &format!(r#"{{"path":"{path}","output":"{out}","row":1}}"#),
    );
    assert_eq!(r["ok"], true, "freeze: {r}");

    // inspect the worksheet xml for a frozen pane
    let bytes = std::fs::read(&out).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut ws = String::new();
    zip.by_name("xl/worksheets/sheet1.xml")
        .unwrap()
        .read_to_string(&mut ws)
        .unwrap();
    assert!(ws.contains("frozen"), "frozen pane present: {ws}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_autofilter_applied() {
    use std::io::Read as _;
    let path = tmp("autofilter.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["a","b"],[1,2],[3,4]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("autofilter_out.xlsx");
    let r = call(
        office__sheet_autofilter,
        &format!(r#"{{"path":"{path}","output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "autofilter: {r}");
    // default range covers all rows/cols: [0,0,2,1]
    assert_eq!(r["range"][2].as_u64().unwrap(), 2, "last row index: {r}");
    assert_eq!(r["range"][3].as_u64().unwrap(), 1, "last col index: {r}");

    let bytes = std::fs::read(&out).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut ws = String::new();
    zip.by_name("xl/worksheets/sheet1.xml")
        .unwrap()
        .read_to_string(&mut ws)
        .unwrap();
    assert!(
        ws.contains("autoFilter"),
        "autoFilter element present: {ws}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_merge_cells_applied() {
    use std::io::Read as _;
    let path = tmp("merge.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["Title","",""],[1,2,3]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // merge the title across A1:C1 (row 0, cols 0..2)
    let out = tmp("merge_out.xlsx");
    let r = call(
        office__sheet_merge_cells,
        &format!(r#"{{"path":"{path}","ranges":[[0,0,0,2]],"output":"{out}"}}"#),
    );
    assert_eq!(r["merged"], 1, "one range merged: {r}");

    // the worksheet XML carries a mergeCells element with the A1:C1 ref
    let bytes = std::fs::read(&out).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut ws = String::new();
    zip.by_name("xl/worksheets/sheet1.xml")
        .unwrap()
        .read_to_string(&mut ws)
        .unwrap();
    assert!(ws.contains("mergeCell"), "mergeCells emitted: {ws}");
    assert!(ws.contains("A1:C1"), "A1:C1 merge ref: {ws}");

    // bad range shape is rejected
    let bad = call(
        office__sheet_merge_cells,
        &format!(r#"{{"path":"{path}","ranges":[[0,0]],"output":"{out}"}}"#),
    );
    assert!(bad["error"].is_string(), "malformed range rejected: {bad}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_autosize_sets_column_widths() {
    use std::io::Read as _;
    let path = tmp("autosize.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["short","a much longer header cell"],[1,2]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("autosize_out.xlsx");
    let r = call(
        office__sheet_autosize,
        &format!(r#"{{"path":"{path}","output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "autosize: {r}");
    assert_eq!(r["sheets"], 1, "one sheet autofit: {r}");

    // autofit emits explicit <col ... width="..."> entries in the worksheet XML.
    let bytes = std::fs::read(&out).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut ws = String::new();
    zip.by_name("xl/worksheets/sheet1.xml")
        .unwrap()
        .read_to_string(&mut ws)
        .unwrap();
    assert!(
        ws.contains("<cols>"),
        "explicit column widths emitted: {ws}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_protect_locks_worksheet() {
    use std::io::Read as _;
    let path = tmp("protect.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["a"],[1]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let read_xml = |p: &str| {
        let bytes = std::fs::read(p).unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut s = String::new();
        zip.by_name("xl/worksheets/sheet1.xml")
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s
    };

    // password-less protection
    let out = tmp("protect_out.xlsx");
    let r = call(
        office__sheet_protect,
        &format!(r#"{{"path":"{path}","output":"{out}"}}"#),
    );
    assert_eq!(r["sheets"], 1, "one sheet protected: {r}");
    assert!(
        read_xml(&out).contains("sheetProtection"),
        "protection element present"
    );

    // password protection records a hash attribute
    let outp = tmp("protect_pw.xlsx");
    let rp = call(
        office__sheet_protect,
        &format!(r#"{{"path":"{path}","output":"{outp}","password":"s3cret"}}"#),
    );
    assert_eq!(rp["ok"], true, "pw protect: {rp}");
    let xml = read_xml(&outp);
    assert!(
        xml.contains("sheetProtection"),
        "pw protection element present"
    );
    assert!(xml.contains("password="), "password hash recorded: {xml}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outp).ok();
}

#[test]
fn sheet_comments_round_trip() {
    // Write a note via the per-sheet `notes` key, then read it back.
    let path = tmp("comments.xlsx");
    let w = call(
        office__sheet_write,
        &serde_json::json!({
            "path": path,
            "sheets": [{
                "name": "D",
                "rows": [["a"], [1]],
                "notes": [{ "row": 0, "col": 0, "text": "needs review", "author": "Jane" }]
            }]
        })
        .to_string(),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(office__sheet_comments, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["count"], 1, "one comment: {r}");
    let c = &r["comments"][0];
    assert_eq!(c["cell"], "A1", "cell ref: {r}");
    assert_eq!(c["author"], "Jane", "author: {r}");
    assert!(
        c["text"].as_str().unwrap().contains("needs review"),
        "comment text: {r}"
    );

    // non-xlsx is rejected
    let csv = tmp("c.csv");
    std::fs::write(&csv, "a\n1\n").unwrap();
    let err = call(office__sheet_comments, &format!(r#"{{"path":"{csv}"}}"#));
    assert!(err["error"].is_string(), "csv rejected: {err}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&csv).ok();
}

#[test]
fn sheet_round_decimals() {
    let path = tmp("round.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["a","b"],[1.234,5.678],[9.871,0.30000000004]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("round_out.xlsx");
    let r = call(
        office__sheet_round,
        &format!(r#"{{"path":"{path}","output":"{out}","decimals":1}}"#),
    );
    assert_eq!(r["ok"], true, "round: {r}");

    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    // Header untouched.
    assert_eq!(rows[0][0], "a");
    // Data rounded to 1 decimal.
    assert_eq!(rows[1][0].as_f64().unwrap(), 1.2, "1.234 -> 1.2: {rd}");
    assert_eq!(rows[1][1].as_f64().unwrap(), 5.7, "5.678 -> 5.7: {rd}");
    assert_eq!(rows[2][0].as_f64().unwrap(), 9.9, "9.871 -> 9.9: {rd}");
    assert_eq!(
        rows[2][1].as_f64().unwrap(),
        0.3,
        "float noise -> 0.3: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_round_columns_subset() {
    let path = tmp("round_cols.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["a","b"],[1.234,5.678]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("round_cols_out.xlsx");
    let r = call(
        office__sheet_round,
        &format!(r#"{{"path":"{path}","output":"{out}","decimals":1,"columns":["a"]}}"#),
    );
    assert_eq!(r["ok"], true, "round: {r}");

    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    // Only column "a" rounded; "b" left as-is.
    assert_eq!(rows[1][0].as_f64().unwrap(), 1.2, "a rounded: {rd}");
    assert_eq!(rows[1][1].as_f64().unwrap(), 5.678, "b untouched: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_histogram_bins() {
    let path = tmp("hist.xlsx");
    // Values 1..=10 across 5 bins → 2 per bin.
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[1],[2],[3],[4],[5],[6],[7],[8],[9],[10]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_histogram,
        &format!(r#"{{"path":"{path}","column":"v","bins":5}}"#),
    );
    assert_eq!(r["count"], 10, "count: {r}");
    assert_eq!(r["min"].as_f64().unwrap(), 1.0, "min: {r}");
    assert_eq!(r["max"].as_f64().unwrap(), 10.0, "max: {r}");
    let bins = r["bins"].as_array().unwrap();
    assert_eq!(bins.len(), 5, "bin count: {r}");
    // width = 9/5 = 1.8; each of the 5 bins gets 2 values, max lands in last bin.
    let total: u64 = bins.iter().map(|b| b["count"].as_u64().unwrap()).sum();
    assert_eq!(total, 10, "all values binned: {r}");
    assert_eq!(bins[0]["lo"].as_f64().unwrap(), 1.0, "first lo: {r}");
    assert_eq!(bins[4]["hi"].as_f64().unwrap(), 10.0, "last hi == max: {r}");
    assert_eq!(
        bins[4]["count"].as_u64().unwrap(),
        2,
        "max in last bin: {r}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_histogram_empty_column() {
    let path = tmp("hist_empty.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],["x"],["y"]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_histogram,
        &format!(r#"{{"path":"{path}","column":"v"}}"#),
    );
    assert_eq!(r["count"], 0, "no numeric values: {r}");
    assert!(r["bins"].as_array().unwrap().is_empty(), "no bins: {r}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_bin_explicit_edges_with_labels() {
    let path = tmp("bin.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["score"],[5],[55],[95]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("bin_out.xlsx");
    // edges 0,50,100 → 2 bins; labels low/high.
    let r = call(
        office__sheet_bin,
        &format!(
            r#"{{"path":"{path}","output":"{out}","column":"score","edges":[0,50,100],"labels":["low","high"],"into":"band"}}"#
        ),
    );
    assert_eq!(r["bins"], 2, "two bins: {r}");
    assert_eq!(r["into"], "band", "new column name: {r}");

    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][1], "band", "header appended: {rd}");
    assert_eq!(rows[1][1], "low", "5 -> low: {rd}");
    assert_eq!(rows[2][1], "high", "55 -> high: {rd}");
    assert_eq!(rows[3][1], "high", "95 (max edge inclusive) -> high: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_bin_equal_width_index() {
    let path = tmp("bin_eq.xlsx");
    let w = call(
        office__sheet_write,
        &format!(r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[0],[5],[10]]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("bin_eq_out.xlsx");
    // 2 equal-width bins over [0,10]: [0,5) and [5,10]. Indices 0,0,1? 5 -> bin 1.
    let r = call(
        office__sheet_bin,
        &format!(r#"{{"path":"{path}","output":"{out}","column":"v","bins":2}}"#),
    );
    assert_eq!(r["bins"], 2, "two bins: {r}");

    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    // bin indices round-trip through xlsx as floats.
    assert_eq!(rows[1][1].as_f64().unwrap(), 0.0, "0 -> bin 0: {rd}");
    assert_eq!(rows[2][1].as_f64().unwrap(), 1.0, "5 -> bin 1: {rd}");
    assert_eq!(
        rows[3][1].as_f64().unwrap(),
        1.0,
        "10 (max) -> last bin: {rd}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_ntile_quartiles() {
    let path = tmp("ntile.xlsx");
    // 8 values, n=4 → 2 per quartile.
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[1],[2],[3],[4],[5],[6],[7],[8]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("ntile_out.xlsx");
    let r = call(
        office__sheet_ntile,
        &format!(r#"{{"path":"{path}","output":"{out}","column":"v","n":4}}"#),
    );
    assert_eq!(r["buckets"], 4, "four buckets: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    // ranks 0,1->bucket0; 2,3->1; 4,5->2; 6,7->3 (values round-trip as floats)
    assert_eq!(rows[1][1].as_f64().unwrap(), 0.0, "1 -> q0: {rd}");
    assert_eq!(rows[2][1].as_f64().unwrap(), 0.0, "2 -> q0: {rd}");
    assert_eq!(rows[3][1].as_f64().unwrap(), 1.0, "3 -> q1: {rd}");
    assert_eq!(rows[5][1].as_f64().unwrap(), 2.0, "5 -> q2: {rd}");
    assert_eq!(rows[8][1].as_f64().unwrap(), 3.0, "8 -> q3: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_outliers_iqr() {
    let path = tmp("outliers.xlsx");
    // Tight cluster 10..14 with one extreme value 100.
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[10],[11],[12],[13],[14],[100]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_outliers,
        &format!(r#"{{"path":"{path}","column":"v"}}"#),
    );
    assert_eq!(r["method"], "iqr", "method: {r}");
    assert_eq!(r["count"], 1, "one outlier: {r}");
    let outs = r["outliers"].as_array().unwrap();
    assert_eq!(
        outs[0]["value"].as_f64().unwrap(),
        100.0,
        "outlier value: {r}"
    );
    // 100 is the 6th data row → 0-based index 5.
    assert_eq!(
        outs[0]["row"].as_u64().unwrap(),
        5,
        "outlier row index: {r}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_outliers_zscore() {
    let path = tmp("outliers_z.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["v"],[1],[1],[1],[1],[1],[1],[1],[1],[1],[50]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_outliers,
        &format!(r#"{{"path":"{path}","column":"v","method":"zscore","k":2}}"#),
    );
    assert_eq!(r["method"], "zscore", "method: {r}");
    assert_eq!(r["count"], 1, "one z-score outlier: {r}");
    assert_eq!(
        r["outliers"][0]["value"].as_f64().unwrap(),
        50.0,
        "outlier value: {r}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_records_round_trip() {
    let path = tmp("rec.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","qty"],["a",1],["b",2]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // read as records
    let r = call(office__sheet_records, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["count"], 2, "two records: {r}");
    assert_eq!(r["fields"][0], "name", "field 0: {r}");
    assert_eq!(r["records"][0]["name"], "a", "rec0 name: {r}");
    assert_eq!(r["records"][1]["qty"], 2.0, "rec1 qty numeric: {r}");

    // write records back out, then re-read — values survive
    let out = tmp("rec_out.xlsx");
    let records = r["records"].clone();
    let ww = call(
        office__records_write,
        &format!(
            r#"{{"path":"{out}","records":{}}}"#,
            serde_json::to_string(&records).unwrap()
        ),
    );
    assert_eq!(ww["ok"], true, "records_write: {ww}");
    assert_eq!(ww["rows"], 2, "wrote 2 rows: {ww}");
    let r2 = call(office__sheet_records, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(r2["records"][0]["name"], "a", "round-trip name: {r2}");
    assert_eq!(r2["records"][1]["qty"], 2.0, "round-trip qty: {r2}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_to_map_key_value() {
    let path = tmp("tomap.xlsx");
    // duplicate key "a" -> last wins (2); blank key row skipped
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[
                ["code","label"],
                ["a","Apple"],
                ["b","Banana"],
                ["a","Avocado"],
                ["","Skip"]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__sheet_to_map,
        &format!(r#"{{"path":"{path}","key":"code","value":"label"}}"#),
    );
    assert_eq!(r["count"], 2, "two distinct keys (blank skipped): {r}");
    assert_eq!(r["map"]["a"], "Avocado", "duplicate key last wins: {r}");
    assert_eq!(r["map"]["b"], "Banana", "second key: {r}");
    assert!(r["map"].get("").is_none(), "blank key skipped: {r}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn sheet_to_ndjson_lines() {
    let path = tmp("nd.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","qty"],["a",1],["b",2]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("nd.jsonl");
    let r = call(
        office__sheet_to_ndjson,
        &format!(r#"{{"path":"{path}","output":"{out}"}}"#),
    );
    assert_eq!(r["count"], 2, "two records: {r}");

    let text = std::fs::read_to_string(&out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "one object per data row: {text:?}");
    // each line is a standalone JSON object keyed by header
    let o0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(o0["name"], "a", "line0 name: {text:?}");
    let o1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(o1["qty"], 2.0, "line1 qty numeric: {text:?}");

    // round-trip: import the .jsonl back into a sheet
    let back = tmp("nd_back.xlsx");
    let ri = call(
        office__ndjson_to_sheet,
        &format!(r#"{{"input":"{out}","output":"{back}"}}"#),
    );
    assert_eq!(ri["ok"], true, "ndjson_to_sheet: {ri}");
    assert_eq!(ri["rows"], 2, "two rows imported: {ri}");
    let rb = call(office__sheet_read, &format!(r#"{{"path":"{back}"}}"#));
    let rows = rb["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "name", "header from keys: {rb}");
    assert_eq!(rows[1][0], "a", "value a: {rb}");
    assert_eq!(rows[2][1].as_f64().unwrap(), 2.0, "numeric survives: {rb}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&back).ok();
}

#[test]
fn sheet_to_xml_export() {
    let path = tmp("toxml.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","age"],["A & B",30],["Carol",25]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let out = tmp("toxml.xml");
    let r = call(
        office__sheet_to_xml,
        &format!(r#"{{"path":"{path}","output":"{out}","root":"people","row":"person"}}"#),
    );
    assert_eq!(r["count"].as_u64().unwrap(), 2, "two rows: {r}");
    let xml = std::fs::read_to_string(&out).unwrap();
    assert!(
        xml.contains("<people>") && xml.contains("</people>"),
        "root tag: {xml}"
    );
    assert!(xml.contains("<person>"), "row tag: {xml}");
    assert!(
        xml.contains("<name>A &amp; B</name>"),
        "value escaped: {xml}"
    );
    assert!(xml.contains("<age>30</age>"), "numeric value: {xml}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn xml_to_sheet_import() {
    // round-trip: build XML inline, parse it back into a sheet
    let xml = r#"<?xml version="1.0"?>
<people>
  <person><name>Alice</name><age>30</age></person>
  <person><name>Bob</name><age>25</age></person>
</people>"#;
    let out = tmp("fromxml.xlsx");
    let r = call(
        office__xml_to_sheet,
        &format!(
            r#"{{"xml":{},"output":"{out}","row":"person"}}"#,
            serde_json::to_string(xml).unwrap()
        ),
    );
    assert_eq!(r["rows"].as_u64().unwrap(), 2, "two rows imported: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    // header from field tags, then data
    let header: Vec<&str> = rows[0]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        header.contains(&"name") && header.contains(&"age"),
        "headers from tags: {rd}"
    );
    let name_col = header.iter().position(|&h| h == "name").unwrap();
    assert_eq!(rows[1][name_col], "Alice", "first row name: {rd}");
    assert_eq!(rows[2][name_col], "Bob", "second row name: {rd}");

    std::fs::remove_file(&out).ok();
}

#[test]
fn ndjson_to_sheet_from_text() {
    // inline ndjson string (blank line skipped)
    let out = tmp("nd_text.xlsx");
    let r = call(
        office__ndjson_to_sheet,
        &serde_json::json!({
            "ndjson": "{\"k\":\"x\",\"n\":1}\n\n{\"k\":\"y\",\"n\":2}\n",
            "output": out
        })
        .to_string(),
    );
    assert_eq!(r["rows"], 2, "blank line skipped, 2 rows: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "k", "first field: {rd}");
    assert_eq!(rows[2][0], "y", "second record: {rd}");

    std::fs::remove_file(&out).ok();
}

#[test]
fn csv_to_sheet_rfc4180() {
    let out = tmp("csv2sheet.xlsx");
    // a quoted field with an embedded comma + numeric coercion
    let r = call(
        office__csv_to_sheet,
        &serde_json::json!({
            "csv": "name,n\n\"a,b\",5\nplain,6\n",
            "output": out
        })
        .to_string(),
    );
    assert_eq!(r["rows"], 3, "header + 2 rows: {r}");
    assert_eq!(r["cols"], 2, "two columns: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[1][0], "a,b", "quoted comma field preserved: {rd}");
    assert_eq!(rows[1][1].as_f64().unwrap(), 5.0, "numeric coerced: {rd}");
    assert_eq!(rows[2][0], "plain", "plain field: {rd}");

    // round-trip with sheet_to_csv
    let back = call(office__sheet_to_csv, &format!(r#"{{"path":"{out}"}}"#));
    assert!(
        back["csv"].as_str().unwrap().contains("\"a,b\",5"),
        "round-trips through to_csv: {back}"
    );

    std::fs::remove_file(&out).ok();
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
fn csv_custom_delimiter() {
    // write a semicolon-delimited .csv
    let path = tmp("semi.csv");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"S","rows":[["a","b"],[1,2]]}}],"delimiter":";"}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("a;b"), "semicolon-separated: {raw:?}");

    // read it back with the same delimiter
    let r = call(
        office__sheet_read,
        &format!(r#"{{"path":"{path}","delimiter":";"}}"#),
    );
    assert_eq!(r["sheets"][0]["rows"][0][0], "a", "header a: {r}");
    assert_eq!(r["sheets"][0]["rows"][0][1], "b", "header b: {r}");
    assert_eq!(r["sheets"][0]["rows"][1][1], 2.0, "numeric: {r}");

    std::fs::remove_file(&path).ok();
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
fn doc_tables_extracted_from_docx_and_odt() {
    // docx: write a table via doc_write, recover its grid via doc_tables
    let dx = tmp("tbl.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"para","text":"Intro"}},
                {{"kind":"table","rows":[["Name","Qty"],["Widget","3"],["Gadget","7"]]}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "docx write: {w}");
    let t = call(office__doc_tables, &format!(r#"{{"path":"{dx}"}}"#));
    assert_eq!(t["count"], 1, "one table found: {t}");
    assert_eq!(t["tables"][0]["rows"][0][0], "Name", "header cell: {t}");
    assert_eq!(t["tables"][0]["rows"][1][1], "3", "body cell: {t}");
    assert_eq!(t["tables"][0]["rows"][2][0], "Gadget", "last row: {t}");

    // odt: hand-build a minimal content.xml with a table inside an odt zip
    let od = tmp("tbl.odt");
    {
        use std::io::Write as _;
        use zip::write::SimpleFileOptions;
        let f = std::fs::File::create(&od).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opt = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zw.start_file("content.xml", opt).unwrap();
        zw.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
  xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
 <office:body><office:text>
  <table:table>
   <table:table-row>
    <table:table-cell><text:p>A1</text:p></table:table-cell>
    <table:table-cell><text:p>B1</text:p></table:table-cell>
   </table:table-row>
   <table:table-row>
    <table:table-cell><text:p>A2</text:p></table:table-cell>
    <table:table-cell><text:p>B2</text:p></table:table-cell>
   </table:table-row>
  </table:table>
 </office:text></office:body>
</office:document-content>"#,
        )
        .unwrap();
        zw.finish().unwrap();
    }
    let ot = call(office__doc_tables, &format!(r#"{{"path":"{od}"}}"#));
    assert_eq!(ot["count"], 1, "odt one table: {ot}");
    assert_eq!(ot["tables"][0]["rows"][0][1], "B1", "odt cell B1: {ot}");
    assert_eq!(ot["tables"][0]["rows"][1][0], "A2", "odt cell A2: {ot}");

    std::fs::remove_file(&dx).ok();
    std::fs::remove_file(&od).ok();
}

#[test]
fn doc_table_to_sheet_extracts_grid() {
    let dx = tmp("t2s.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"para","text":"before"}},
                {{"kind":"table","rows":[["Name","Qty"],["Widget","3"],["Gadget","7"]]}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "docx write: {w}");

    let out = tmp("t2s.xlsx");
    let r = call(
        office__doc_table_to_sheet,
        &format!(r#"{{"path":"{dx}","output":"{out}","name":"Grid"}}"#),
    );
    assert_eq!(r["ok"], true, "extract: {r}");
    assert_eq!(r["rows"], 3, "three rows: {r}");
    assert_eq!(r["cols"], 2, "two columns: {r}");

    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let sheet = &rd["sheets"][0];
    assert_eq!(sheet["name"], "Grid", "sheet name carried: {rd}");
    let rows = sheet["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "Name", "header cell: {rd}");
    assert_eq!(rows[2][0], "Gadget", "last row: {rd}");

    std::fs::remove_file(&dx).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn sheet_to_doc_renders_table() {
    let xl = tmp("s2d_in.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{xl}","sheets":[{{"name":"S","rows":[
                ["Item","Count"],
                ["apples",5],
                ["pears",12]
            ]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("s2d_out.docx");
    let r = call(
        office__sheet_to_doc,
        &format!(r#"{{"path":"{xl}","output":"{out}","title":"Inventory"}}"#),
    );
    assert_eq!(r["ok"], true, "render: {r}");
    assert_eq!(r["rows"], 3, "three rows: {r}");
    assert_eq!(r["cols"], 2, "two cols: {r}");

    // heading carried + table grid recoverable with integers (no ".0")
    let ol = call(office__doc_outline, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(ol["outline"][0]["text"], "Inventory", "title heading: {ol}");
    let t = call(office__doc_tables, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(t["count"], 1, "one table: {t}");
    assert_eq!(t["tables"][0]["rows"][0][0], "Item", "header: {t}");
    assert_eq!(
        t["tables"][0]["rows"][1][1], "5",
        "integer cell, no .0: {t}"
    );
    assert_eq!(t["tables"][0]["rows"][2][0], "pears", "last row: {t}");

    std::fs::remove_file(&xl).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn doc_stats_word_count_across_formats() {
    // docx: heading "Title" (1 word) + paragraph "one two three four" (4 words)
    let dx = tmp("stats.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"heading","level":1,"text":"Title"}},
                {{"kind":"para","text":"one two three four"}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "docx write: {w}");
    let s = call(office__doc_stats, &format!(r#"{{"path":"{dx}"}}"#));
    assert_eq!(s["words"], 5, "5 words: {s}");
    assert_eq!(s["paragraphs"], 2, "2 paragraphs: {s}");
    // "Title"(5) + "onetwothreefour"(15) = 20 non-space chars
    assert_eq!(s["characters_no_spaces"], 20, "non-space chars: {s}");

    // plain text
    let tx = tmp("stats.txt");
    std::fs::write(&tx, "hello world\nfoo bar baz").unwrap();
    let st = call(office__doc_stats, &format!(r#"{{"path":"{tx}"}}"#));
    assert_eq!(st["words"], 5, "txt words: {st}");
    assert_eq!(st["lines"], 2, "txt lines: {st}");

    std::fs::remove_file(&dx).ok();
    std::fs::remove_file(&tx).ok();
}

#[test]
fn doc_merge_concatenates_and_converts() {
    // two source docx
    let a = tmp("merge_a.docx");
    let b = tmp("merge_b.docx");
    for (path, head, body) in [(&a, "Alpha", "first doc"), (&b, "Bravo", "second doc")] {
        let w = call(
            office__doc_write,
            &format!(
                r#"{{"path":"{path}","blocks":[
                    {{"kind":"heading","level":1,"text":"{head}"}},
                    {{"kind":"para","text":"{body}"}}
                ]}}"#
            ),
        );
        assert_eq!(w["ok"], true, "write {head}: {w}");
    }

    // merge into a docx
    let merged = tmp("merged.docx");
    let m = call(
        office__doc_merge,
        &format!(r#"{{"inputs":["{a}","{b}"],"output":"{merged}"}}"#),
    );
    assert_eq!(m["ok"], true, "merge: {m}");
    assert_eq!(m["sources"], 2, "two sources: {m}");
    // both documents' content present: 1+2+1+2 = 6 words
    let s = call(office__doc_stats, &format!(r#"{{"path":"{merged}"}}"#));
    assert_eq!(s["words"], 6, "merged word count: {s}");
    let paras = call(office__doc_read, &format!(r#"{{"path":"{merged}"}}"#));
    let joined = paras["paragraphs"].to_string();
    assert!(
        joined.contains("Alpha") && joined.contains("Bravo"),
        "both heads: {joined}"
    );

    // merge doubles as conversion: same sources -> markdown
    let md = tmp("merged.md");
    let mm = call(
        office__doc_merge,
        &format!(r#"{{"inputs":["{a}","{b}"],"output":"{md}","page_breaks":false}}"#),
    );
    assert_eq!(mm["ok"], true, "merge->md: {mm}");
    let text = std::fs::read_to_string(&md).unwrap();
    assert!(
        text.contains("# Alpha") && text.contains("# Bravo"),
        "md headings: {text}"
    );

    for f in [&a, &b, &merged, &md] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn slides_append_adds_slides() {
    let path = tmp("sapp.pptx");
    let w = call(
        office__slides_write,
        &format!(r#"{{"path":"{path}","slides":[{{"title":"A","body":["x"]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("sapp_out.pptx");
    let r = call(
        office__slides_append,
        &format!(r#"{{"path":"{path}","slides":[{{"title":"B","body":["y"]}}],"output":"{out}"}}"#),
    );
    assert_eq!(r["added"], 1, "added 1 slide: {r}");
    assert_eq!(r["slides"], 2, "2 slides total: {r}");
    let rd = call(office__slides_read, &format!(r#"{{"path":"{out}"}}"#));
    let slides = rd["slides"].as_array().unwrap();
    assert_eq!(slides.len(), 2, "read 2 slides: {rd}");
    assert_eq!(slides[0]["text"][0], "A", "first title: {rd}");
    assert_eq!(slides[1]["text"][0], "B", "appended title: {rd}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn slides_stats_counts() {
    let px = tmp("sstats.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{px}","slides":[
                {{"title":"Intro","body":["one two","three"]}},
                {{"title":"Next","body":["four"]}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let s = call(office__slides_stats, &format!(r#"{{"path":"{px}"}}"#));
    assert_eq!(s["slides"], 2, "two slides: {s}");
    // slide0: Intro(1)+one two(2)+three(1)=4 ; slide1: Next(1)+four(1)=2 ; total 6
    assert_eq!(s["words"], 6, "total text words: {s}");
    assert_eq!(s["notes_words"], 0, "no notes: {s}");
    assert_eq!(s["per_slide"][0]["words"], 4, "slide0 words: {s}");
    assert_eq!(s["per_slide"][1]["words"], 2, "slide1 words: {s}");

    std::fs::remove_file(&px).ok();
}

#[test]
fn slides_merge_combines_decks() {
    let a = tmp("deckA.pptx");
    let b = tmp("deckB.pptx");
    let wa = call(
        office__slides_write,
        &format!(r#"{{"path":"{a}","slides":[{{"title":"A1","body":["abody"]}}]}}"#),
    );
    assert_eq!(wa["ok"], true, "deck A: {wa}");
    let wb = call(
        office__slides_write,
        &format!(r#"{{"path":"{b}","slides":[{{"title":"B1","body":["bbody"]}}]}}"#),
    );
    assert_eq!(wb["ok"], true, "deck B: {wb}");

    let merged = tmp("deckM.pptx");
    let m = call(
        office__slides_merge,
        &format!(r#"{{"inputs":["{a}","{b}"],"output":"{merged}"}}"#),
    );
    assert_eq!(m["slides"], 2, "two merged slides: {m}");
    let rd = call(office__slides_read, &format!(r#"{{"path":"{merged}"}}"#));
    let slides = rd["slides"].as_array().unwrap();
    assert_eq!(slides.len(), 2, "read 2 slides: {rd}");
    assert_eq!(slides[0]["text"][0], "A1", "first slide title: {rd}");
    assert_eq!(slides[1]["text"][0], "B1", "second slide title: {rd}");

    for f in [&a, &b, &merged] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn slides_reorder_and_subset() {
    let deck = tmp("reorder.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{deck}","slides":[{{"title":"A","body":["a"]}},{{"title":"B","body":["b"]}},{{"title":"C","body":["c"]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // reorder to [3,1] — drops slide 2, swaps order
    let out = tmp("reorder_out.pptx");
    let r = call(
        office__slides_reorder,
        &format!(r#"{{"path":"{deck}","order":[3,1],"output":"{out}"}}"#),
    );
    assert_eq!(r["slides"], 2, "two slides kept: {r}");
    let rd = call(office__slides_read, &format!(r#"{{"path":"{out}"}}"#));
    let slides = rd["slides"].as_array().unwrap();
    assert_eq!(slides.len(), 2, "read 2 slides: {rd}");
    assert_eq!(slides[0]["text"][0], "C", "first is C: {rd}");
    assert_eq!(slides[1]["text"][0], "A", "second is A: {rd}");

    std::fs::remove_file(&deck).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn slides_delete_by_number() {
    let deck = tmp("sdel.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{deck}","slides":[{{"title":"A","body":["a"]}},{{"title":"B","body":["b"]}},{{"title":"C","body":["c"]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // delete slide 2 (B) → A, C remain
    let out = tmp("sdel_out.pptx");
    let r = call(
        office__slides_delete,
        &format!(r#"{{"path":"{deck}","slides":[2],"output":"{out}"}}"#),
    );
    assert_eq!(r["removed"], 1, "one removed: {r}");
    assert_eq!(r["slides"], 2, "two remain: {r}");
    let rd = call(office__slides_read, &format!(r#"{{"path":"{out}"}}"#));
    let slides = rd["slides"].as_array().unwrap();
    assert_eq!(slides.len(), 2, "read 2 slides: {rd}");
    assert_eq!(slides[0]["text"][0], "A", "first is A: {rd}");
    assert_eq!(slides[1]["text"][0], "C", "second is C: {rd}");

    // refusing to delete every slide
    let all = call(
        office__slides_delete,
        &format!(r#"{{"path":"{deck}","slides":[1,2,3],"output":"{out}"}}"#),
    );
    assert!(all["error"].is_string(), "deleting all is rejected: {all}");

    std::fs::remove_file(&deck).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn slides_insert_at_position() {
    let deck = tmp("sins.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{deck}","slides":[{{"title":"A","body":["a"]}},{{"title":"C","body":["c"]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // insert "B" at position 2 → A, B, C
    let out = tmp("sins_out.pptx");
    let r = call(
        office__slides_insert,
        &format!(r#"{{"path":"{deck}","position":2,"title":"B","body":["b"],"output":"{out}"}}"#),
    );
    assert_eq!(r["slides"], 3, "three slides: {r}");
    assert_eq!(r["position"], 2, "inserted at position 2: {r}");
    let rd = call(office__slides_read, &format!(r#"{{"path":"{out}"}}"#));
    let slides = rd["slides"].as_array().unwrap();
    assert_eq!(slides[0]["text"][0], "A", "first A: {rd}");
    assert_eq!(slides[1]["text"][0], "B", "second B (inserted): {rd}");
    assert_eq!(slides[2]["text"][0], "C", "third C: {rd}");

    // no position → append at end
    let outa = tmp("sins_app.pptx");
    let ra = call(
        office__slides_insert,
        &format!(r#"{{"path":"{deck}","title":"Z","output":"{outa}"}}"#),
    );
    assert_eq!(ra["position"], 3, "appended at end (deck had 2): {ra}");

    std::fs::remove_file(&deck).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outa).ok();
}

#[test]
fn slides_set_title_replaces() {
    let deck = tmp("settitle.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{deck}","slides":[{{"title":"Old","body":["keep me"]}},{{"title":"Two","body":["b"]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("settitle_out.pptx");
    let r = call(
        office__slides_set_title,
        &format!(r#"{{"path":"{deck}","slide":1,"title":"New Title","output":"{out}"}}"#),
    );
    assert_eq!(r["slide"], 1, "slide number echoed: {r}");
    let rd = call(office__slides_read, &format!(r#"{{"path":"{out}"}}"#));
    let slides = rd["slides"].as_array().unwrap();
    assert_eq!(slides[0]["text"][0], "New Title", "title replaced: {rd}");
    assert_eq!(slides[0]["text"][1], "keep me", "body preserved: {rd}");
    assert_eq!(slides[1]["text"][0], "Two", "other slide untouched: {rd}");

    // out-of-range slide errors
    let bad = call(
        office__slides_set_title,
        &format!(r#"{{"path":"{deck}","slide":9,"title":"X","output":"{out}"}}"#),
    );
    assert!(bad["error"].is_string(), "out-of-range rejected: {bad}");

    std::fs::remove_file(&deck).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn slides_set_body_replaces() {
    let deck = tmp("setbody.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{deck}","slides":[{{"title":"Keep","body":["old1","old2"]}},{{"title":"Two","body":["b"]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("setbody_out.pptx");
    let r = call(
        office__slides_set_body,
        &serde_json::json!({
            "path": deck, "slide": 1, "body": ["new line one", "new line two"], "output": out
        })
        .to_string(),
    );
    assert_eq!(r["slide"], 1, "slide echoed: {r}");
    let rd = call(office__slides_read, &format!(r#"{{"path":"{out}"}}"#));
    let slides = rd["slides"].as_array().unwrap();
    assert_eq!(slides[0]["text"][0], "Keep", "title preserved: {rd}");
    assert_eq!(slides[0]["text"][1], "new line one", "body replaced: {rd}");
    assert_eq!(slides[0]["text"][2], "new line two", "body line 2: {rd}");
    assert_eq!(slides[1]["text"][1], "b", "other slide untouched: {rd}");

    std::fs::remove_file(&deck).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn slides_split_one_file_per_slide() {
    let deck = tmp("splitdeck.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{deck}","slides":[{{"title":"One","body":["b1"]}},{{"title":"Two","body":["b2"]}},{{"title":"Three","body":["b3"]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let dir = tmp("ssplit_out");
    std::fs::create_dir_all(&dir).unwrap();
    let r = call(
        office__slides_split,
        &format!(r#"{{"path":"{deck}","dir":"{dir}","prefix":"s"}}"#),
    );
    assert_eq!(r["count"], 3, "3 slides -> 3 files: {r}");
    let files = r["files"].as_array().unwrap();
    assert_eq!(files.len(), 3, "three file paths: {r}");

    // Each split file is a single-slide deck preserving title + body.
    let rd = call(office__slides_read, &format!(r#"{{"path":{}}}"#, files[1]));
    let slides = rd["slides"].as_array().unwrap();
    assert_eq!(slides.len(), 1, "second file holds one slide: {rd}");
    assert_eq!(slides[0]["text"][0], "Two", "title carried: {rd}");
    assert_eq!(slides[0]["text"][1], "b2", "body carried: {rd}");

    std::fs::remove_file(&deck).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn slides_add_image_embeds_picture() {
    let deck = tmp("imgdeck.pptx");
    let w = call(
        office__slides_write,
        &format!(r#"{{"path":"{deck}","slides":[{{"title":"Cover","body":["x"]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "deck write: {w}");

    // make a 20x10 png to embed
    let png = tmp("logo.png");
    let n = call(
        office__img_new,
        r#"{"width":20,"height":10,"color":[0,128,255,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    call(
        office__img_save,
        &format!(r#"{{"handle":{h},"path":"{png}"}}"#),
    );
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let r = call(
        office__slides_add_image,
        &format!(r#"{{"path":"{deck}","image":"{png}","slide":1}}"#),
    );
    assert_eq!(r["ok"], true, "add_image: {r}");

    // the embedded media is recoverable via extract_images at its native size
    let ex = call(office__extract_images, &format!(r#"{{"path":"{deck}"}}"#));
    assert_eq!(ex["count"], 1, "one embedded image: {ex}");
    assert_eq!(ex["images"][0]["width"], 20, "image width: {ex}");
    assert_eq!(ex["images"][0]["height"], 10, "image height: {ex}");

    // deck still parses: slide text intact (no corruption from the injection)
    let rd = call(office__slides_read, &format!(r#"{{"path":"{deck}"}}"#));
    assert_eq!(
        rd["slides"][0]["text"][0], "Cover",
        "title still reads: {rd}"
    );

    std::fs::remove_file(&deck).ok();
    std::fs::remove_file(&png).ok();
}

#[test]
fn slides_set_notes_round_trip() {
    let deck = tmp("notesdeck.pptx");
    let w = call(
        office__slides_write,
        &format!(r#"{{"path":"{deck}","slides":[{{"title":"Talk","body":["point"]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "deck write: {w}");

    // add notes, then recover them via slides_read's notesSlide path
    let r = call(
        office__slides_set_notes,
        &format!(r#"{{"path":"{deck}","slide":1,"notes":"remember to smile"}}"#),
    );
    assert_eq!(r["ok"], true, "set_notes: {r}");
    assert_eq!(r["lines"], 1, "one note line: {r}");

    let rd = call(office__slides_read, &format!(r#"{{"path":"{deck}"}}"#));
    let notes = rd["slides"][0]["notes"].to_string();
    assert!(
        notes.contains("remember to smile"),
        "notes round-trip: {rd}"
    );
    assert_eq!(rd["slides"][0]["text"][0], "Talk", "title intact: {rd}");

    // replacing notes updates in place (no duplicate notesSlide)
    let r2 = call(
        office__slides_set_notes,
        &format!(r#"{{"path":"{deck}","slide":1,"notes":["line a","line b"]}}"#),
    );
    assert_eq!(r2["lines"], 2, "two lines: {r2}");
    let rd2 = call(office__slides_read, &format!(r#"{{"path":"{deck}"}}"#));
    let n2 = rd2["slides"][0]["notes"].to_string();
    assert!(
        n2.contains("line a") && n2.contains("line b"),
        "replaced notes: {rd2}"
    );
    assert!(!n2.contains("smile"), "old notes replaced: {rd2}");

    std::fs::remove_file(&deck).ok();
}

#[test]
fn slides_add_text_injects_box() {
    let deck = tmp("addtext.pptx");
    let w = call(
        office__slides_write,
        &format!(r#"{{"path":"{deck}","slides":[{{"title":"T","body":["b"]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(
        office__slides_add_text,
        &format!(r#"{{"path":"{deck}","text":"Caption here","slide":1,"x":100,"y":400}}"#),
    );
    assert_eq!(r["ok"], true, "add_text: {r}");

    // the new text box is recoverable via slides_read; existing title intact
    let rd = call(office__slides_read, &format!(r#"{{"path":"{deck}"}}"#));
    let txt = rd["slides"][0]["text"].to_string();
    assert!(txt.contains("Caption here"), "added text present: {rd}");
    assert!(txt.contains("T"), "title still present: {rd}");

    std::fs::remove_file(&deck).ok();
}

#[test]
fn slides_to_pdf_one_per_page() {
    let px = tmp("s2pdf.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{px}","slides":[{{"title":"A","body":["a1"]}},{{"title":"B","body":["b1"]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("s2pdf.pdf");
    let r = call(
        office__slides_to_pdf,
        &format!(r#"{{"path":"{px}","output":"{out}"}}"#),
    );
    assert_eq!(r["slides"], 2, "two slides: {r}");
    let info = call(office__pdf_info, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(info["pages"], 2, "one page per slide: {info}");
    let rd = call(office__pdf_read, &format!(r#"{{"path":"{out}"}}"#));
    let txt = rd["text"].as_str().unwrap_or("");
    assert!(
        txt.contains("A") && txt.contains("a1") && txt.contains("B") && txt.contains("b1"),
        "content: {txt:?}"
    );

    std::fs::remove_file(&px).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn slides_to_doc_round() {
    let px = tmp("s2d.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{px}","slides":[{{"title":"A","body":["a1","a2"]}},{{"title":"B","body":["b1"]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("s2d.docx");
    let r = call(
        office__slides_to_doc,
        &format!(r#"{{"path":"{px}","output":"{out}"}}"#),
    );
    assert_eq!(r["slides"], 2, "two slides consumed: {r}");
    let ol = call(office__doc_outline, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(ol["count"], 2, "two headings: {ol}");
    assert_eq!(ol["outline"][0]["text"], "A", "first heading: {ol}");
    assert_eq!(ol["outline"][1]["text"], "B", "second heading: {ol}");
    let rd = call(office__doc_read, &format!(r#"{{"path":"{out}"}}"#));
    let joined = rd["paragraphs"].to_string();
    assert!(
        joined.contains("a1") && joined.contains("a2") && joined.contains("b1"),
        "bodies: {joined}"
    );

    std::fs::remove_file(&px).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn slides_outline_lists_titles() {
    let px = tmp("soutline.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{px}","slides":[{{"title":"Intro","body":["x"]}},{{"title":"Details","body":["y","z"]}},{{"title":"End","body":[]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let o = call(office__slides_outline, &format!(r#"{{"path":"{px}"}}"#));
    assert_eq!(o["count"], 3, "three slides: {o}");
    assert_eq!(o["outline"][0]["slide"], 1, "1-based: {o}");
    assert_eq!(o["outline"][0]["title"], "Intro", "first title: {o}");
    assert_eq!(o["outline"][1]["title"], "Details", "second title: {o}");
    assert_eq!(o["outline"][2]["title"], "End", "third title: {o}");

    std::fs::remove_file(&px).ok();
}

#[test]
fn slides_to_md_outline() {
    let px = tmp("s2md.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{px}","slides":[{{"title":"Intro","body":["a","b"]}},{{"title":"End","body":[]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(office__slides_to_md, &format!(r#"{{"path":"{px}"}}"#));
    assert_eq!(r["slides"], 2, "two slides: {r}");
    let md = r["markdown"].as_str().unwrap();
    assert!(md.contains("## Intro"), "title heading: {md}");
    assert!(md.contains("- a"), "first bullet: {md}");
    assert!(md.contains("- b"), "second bullet: {md}");
    assert!(md.contains("## End"), "second slide heading: {md}");

    // custom heading level
    let r3 = call(
        office__slides_to_md,
        &format!(r#"{{"path":"{px}","level":3}}"#),
    );
    assert!(
        r3["markdown"].as_str().unwrap().contains("### Intro"),
        "level 3 heading: {r3}"
    );

    std::fs::remove_file(&px).ok();
}

#[test]
fn slides_to_html_sections() {
    let px = tmp("s2html.pptx");
    let w = call(
        office__slides_write,
        &format!(r#"{{"path":"{px}","slides":[{{"title":"Intro","body":["alpha","beta"]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(office__slides_to_html, &format!(r#"{{"path":"{px}"}}"#));
    assert_eq!(r["slides"], 1, "one slide: {r}");
    let html = r["html"].as_str().unwrap();
    assert!(html.contains("<section>"), "section wrapper: {html}");
    assert!(html.contains("<h2>Intro</h2>"), "title h2: {html}");
    assert!(html.contains("<li>alpha</li>"), "first bullet: {html}");
    assert!(html.contains("<li>beta</li>"), "second bullet: {html}");

    std::fs::remove_file(&px).ok();
}

#[test]
fn slides_to_text_extracts() {
    let px = tmp("s2txt.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{px}","slides":[{{"title":"Intro","body":["alpha","beta"]}},{{"title":"End","body":[]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(office__slides_to_text, &format!(r#"{{"path":"{px}"}}"#));
    assert_eq!(r["slides"], 2, "two slides: {r}");
    let text = r["text"].as_str().unwrap();
    assert!(text.contains("Intro"), "first title: {text:?}");
    assert!(text.contains("alpha"), "body line: {text:?}");
    assert!(text.contains("End"), "second slide: {text:?}");

    std::fs::remove_file(&px).ok();
}

#[test]
fn slides_to_sheet_one_row_per_slide() {
    let px = tmp("s2sheet.pptx");
    let w = call(
        office__slides_write,
        &format!(
            r#"{{"path":"{px}","slides":[{{"title":"Intro","body":["a","b"]}},{{"title":"End","body":[]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("s2sheet.xlsx");
    let r = call(
        office__slides_to_sheet,
        &format!(r#"{{"path":"{px}","output":"{out}"}}"#),
    );
    assert_eq!(r["slides"], 2, "two slides: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "slide", "header slide: {rd}");
    assert_eq!(rows[0][1], "title", "header title: {rd}");
    assert_eq!(rows[1][0].as_f64().unwrap(), 1.0, "slide 1 number: {rd}");
    assert_eq!(rows[1][1], "Intro", "slide 1 title: {rd}");
    assert_eq!(rows[1][2], "a b", "slide 1 body joined: {rd}");
    assert_eq!(rows[2][1], "End", "slide 2 title: {rd}");

    std::fs::remove_file(&px).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn pdf_to_sheet_extracts_lines() {
    let pdf = tmp("p2sheet.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{pdf}","elements":[
                {{"type":"paragraph","text":"alpha line"}},
                {{"type":"pagebreak"}},
                {{"type":"paragraph","text":"beta line"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    let out = tmp("p2sheet.xlsx");
    let r = call(
        office__pdf_to_sheet,
        &format!(r#"{{"path":"{pdf}","output":"{out}"}}"#),
    );
    assert!(
        r["rows"].as_u64().unwrap() >= 2,
        "at least 2 data rows: {r}"
    );
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "page", "header page: {rd}");
    let joined = rd["sheets"][0]["rows"].to_string();
    assert!(joined.contains("alpha"), "page 1 text present: {joined}");
    assert!(joined.contains("beta"), "page 2 text present: {joined}");
    // the beta line carries page number 2
    let beta = rows.iter().find(|r| r[1] == "beta line").unwrap();
    assert_eq!(beta[0].as_f64().unwrap(), 2.0, "beta on page 2: {rd}");

    std::fs::remove_file(&pdf).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn doc_to_sheet_one_row_per_block() {
    let dx = tmp("d2sheet.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"heading","level":1,"text":"Title"}},
                {{"kind":"para","text":"body text"}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("d2sheet.xlsx");
    let r = call(
        office__doc_to_sheet,
        &format!(r#"{{"path":"{dx}","output":"{out}"}}"#),
    );
    assert!(
        r["rows"].as_u64().unwrap() >= 2,
        "at least 2 data rows: {r}"
    );
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0][0], "level", "header level: {rd}");
    assert_eq!(rows[1][0].as_f64().unwrap(), 1.0, "heading level 1: {rd}");
    assert_eq!(rows[1][1], "Title", "heading text: {rd}");
    assert_eq!(rows[2][0].as_f64().unwrap(), 0.0, "body level 0: {rd}");
    assert_eq!(rows[2][1], "body text", "body text: {rd}");

    std::fs::remove_file(&dx).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn xml_entities_survive_text_extraction() {
    // Regression: quick-xml emits `&amp;` etc. as standalone GeneralRef events,
    // not inside Text, so the readers must resolve them or `&`/`<`/`>` vanish.

    // pptx slide text (extract_paragraphs path)
    let px = tmp("ent.pptx");
    let w = call(
        office__slides_write,
        &format!(r#"{{"path":"{px}","slides":[{{"title":"R&D <ok>","body":["a & b"]}}]}}"#),
    );
    assert_eq!(w["ok"], true, "slides write: {w}");
    let rd = call(office__slides_read, &format!(r#"{{"path":"{px}"}}"#));
    let joined = rd["slides"][0]["text"].to_string();
    assert!(joined.contains("R&D"), "title amp survives: {joined}");
    assert!(
        joined.contains("<ok>"),
        "title angle brackets survive: {joined}"
    );
    assert!(joined.contains("a & b"), "body amp survives: {joined}");
    std::fs::remove_file(&px).ok();

    // docx paragraph + table cell (extract_paragraphs + extract_tables paths)
    let dx = tmp("ent.docx");
    let w2 = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"para","text":"Tom & Jerry"}},
                {{"kind":"table","rows":[["A & B","x < y"]]}}
            ]}}"#
        ),
    );
    assert_eq!(w2["ok"], true, "doc write: {w2}");
    let dr = call(office__doc_read, &format!(r#"{{"path":"{dx}"}}"#));
    assert!(
        dr["paragraphs"].to_string().contains("Tom & Jerry"),
        "docx para amp survives: {dr}"
    );
    let tb = call(office__doc_tables, &format!(r#"{{"path":"{dx}"}}"#));
    assert_eq!(
        tb["tables"][0]["rows"][0][0], "A & B",
        "table cell amp: {tb}"
    );
    assert_eq!(
        tb["tables"][0]["rows"][0][1], "x < y",
        "table cell lt: {tb}"
    );
    std::fs::remove_file(&dx).ok();
}

#[test]
fn md_to_slides_from_outline() {
    // Preamble before the first heading is dropped; bullets and plain lines
    // become body items.
    let md = "intro paragraph\n\n# One\n\n- a\n- b\n\n# Two\n\n* c\nplain line";
    let out = tmp("md2s.pptx");
    let r = call(
        office__md_to_slides,
        &serde_json::json!({ "markdown": md, "output": out }).to_string(),
    );
    assert_eq!(r["ok"], true, "convert: {r}");
    assert_eq!(r["slides"], 2, "two slides: {r}");

    let rd = call(office__slides_read, &format!(r#"{{"path":"{out}"}}"#));
    let slides = rd["slides"].as_array().unwrap();
    assert_eq!(slides.len(), 2, "read two slides: {rd}");
    assert_eq!(slides[0]["text"][0], "One", "first title: {rd}");
    let s0 = slides[0]["text"].to_string();
    assert!(s0.contains("a") && s0.contains("b"), "first body: {rd}");
    assert_eq!(slides[1]["text"][0], "Two", "second title: {rd}");
    let s1 = slides[1]["text"].to_string();
    assert!(
        s1.contains("c") && s1.contains("plain line"),
        "second body (bullet + plain): {rd}"
    );

    std::fs::remove_file(&out).ok();
}

#[test]
fn doc_to_slides_from_headings() {
    let dx = tmp("d2s.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"heading","level":1,"text":"Slide One"}},
                {{"kind":"para","text":"point a"}},
                {{"kind":"para","text":"point b"}},
                {{"kind":"heading","level":1,"text":"Slide Two"}},
                {{"kind":"para","text":"point c"}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("d2s.pptx");
    let r = call(
        office__doc_to_slides,
        &format!(r#"{{"path":"{dx}","output":"{out}"}}"#),
    );
    assert_eq!(r["slides"], 2, "two slides: {r}");
    let rd = call(office__slides_read, &format!(r#"{{"path":"{out}"}}"#));
    let slides = rd["slides"].as_array().unwrap();
    assert_eq!(slides.len(), 2, "read 2 slides: {rd}");
    let s0 = slides[0]["text"].to_string();
    assert!(
        s0.contains("Slide One") && s0.contains("point a") && s0.contains("point b"),
        "slide0: {s0}"
    );
    let s1 = slides[1]["text"].to_string();
    assert!(
        s1.contains("Slide Two") && s1.contains("point c"),
        "slide1: {s1}"
    );

    std::fs::remove_file(&dx).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn doc_to_text_extracts() {
    let dx = tmp("totxt.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[{{"kind":"para","text":"hello"}},{{"kind":"para","text":"world"}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let out = tmp("totxt.txt");
    let r = call(
        office__doc_to_text,
        &format!(r#"{{"path":"{dx}","output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "doc_to_text: {r}");
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(
        text.contains("hello") && text.contains("world"),
        "extracted text: {text:?}"
    );

    std::fs::remove_file(&dx).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn pdf_to_doc_converts_text() {
    // Build a 2-page PDF, then convert it back to a docx.
    let pdf = tmp("p2d.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{pdf}","elements":[
                {{"type":"paragraph","text":"alpha line"}},
                {{"type":"pagebreak"}},
                {{"type":"paragraph","text":"delta line"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    let out = tmp("p2d.docx");
    let r = call(
        office__pdf_to_doc,
        &format!(r#"{{"path":"{pdf}","output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "convert: {r}");
    assert_eq!(r["pages"], 2, "two pages: {r}");
    assert!(r["paragraphs"].as_u64().unwrap() >= 2, "paragraphs: {r}");

    let rd = call(office__doc_read, &format!(r#"{{"path":"{out}"}}"#));
    let joined = rd["paragraphs"].to_string();
    assert!(
        joined.contains("alpha") && joined.contains("delta"),
        "both pages carried into doc: {joined}"
    );

    std::fs::remove_file(&pdf).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn pdf_to_slides_one_per_page() {
    let pdf = tmp("p2s.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{pdf}","elements":[
                {{"type":"heading","level":1,"text":"First"}},
                {{"type":"paragraph","text":"body one"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Second"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    let out = tmp("p2s.pptx");
    let r = call(
        office__pdf_to_slides,
        &format!(r#"{{"path":"{pdf}","output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "convert: {r}");
    assert_eq!(r["slides"], 2, "two slides: {r}");

    let rd = call(office__slides_read, &format!(r#"{{"path":"{out}"}}"#));
    let slides = rd["slides"].as_array().unwrap();
    assert_eq!(slides.len(), 2, "read two slides: {rd}");
    assert_eq!(slides[0]["text"][0], "First", "page 1 title: {rd}");
    assert_eq!(slides[1]["text"][0], "Second", "page 2 title: {rd}");

    std::fs::remove_file(&pdf).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn doc_to_pdf_renders_blocks() {
    let dx = tmp("d2pdf.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"heading","level":1,"text":"Chapter"}},
                {{"kind":"para","text":"intro body"}},
                {{"kind":"table","rows":[["K","V"],["x","1"]]}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "doc write: {w}");

    let out = tmp("d2pdf.pdf");
    let r = call(
        office__doc_to_pdf,
        &format!(r#"{{"path":"{dx}","output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "render: {r}");
    assert!(r["elements"].as_u64().unwrap() >= 3, "elements mapped: {r}");

    // text extractable back from the produced PDF
    let pr = call(office__pdf_read, &format!(r#"{{"path":"{out}"}}"#));
    let text = pr["text"].as_str().unwrap();
    assert!(text.contains("Chapter"), "heading in pdf: {text:?}");
    assert!(text.contains("intro body"), "paragraph in pdf: {text:?}");

    std::fs::remove_file(&dx).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn html_to_pdf_renders() {
    let html = tmp("h2p.html");
    std::fs::write(
        &html,
        "<h1>Title</h1><p>hello world</p><ul><li>one</li><li>two</li></ul>",
    )
    .unwrap();

    let out = tmp("h2p.pdf");
    let r = call(
        office__html_to_pdf,
        &format!(r#"{{"input":"{html}","output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "convert: {r}");
    assert!(r["elements"].as_u64().unwrap() >= 3, "elements mapped: {r}");

    let pr = call(office__pdf_read, &format!(r#"{{"path":"{out}"}}"#));
    let text = pr["text"].as_str().unwrap();
    assert!(text.contains("Title"), "heading in pdf: {text:?}");
    assert!(text.contains("hello world"), "paragraph in pdf: {text:?}");
    assert!(text.contains("one"), "list item in pdf: {text:?}");

    std::fs::remove_file(&html).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn html_to_sheet_parses_table() {
    let html = tmp("h2sheet.html");
    std::fs::write(
        &html,
        "<p>intro</p><table><tr><td>Name</td><td>Qty</td></tr><tr><td>apple</td><td>5</td></tr></table>",
    )
    .unwrap();

    let out = tmp("h2sheet.xlsx");
    let r = call(
        office__html_to_sheet,
        &format!(r#"{{"input":"{html}","output":"{out}","name":"T"}}"#),
    );
    assert_eq!(r["ok"], true, "parse: {r}");
    assert_eq!(r["rows"], 2, "two table rows: {r}");
    assert_eq!(r["cols"], 2, "two columns: {r}");
    let rd = call(office__sheet_read, &format!(r#"{{"path":"{out}"}}"#));
    let rows = rd["sheets"][0]["rows"].as_array().unwrap();
    assert_eq!(rd["sheets"][0]["name"], "T", "sheet name: {rd}");
    assert_eq!(rows[0][0], "Name", "header cell: {rd}");
    assert_eq!(rows[1][0], "apple", "body cell: {rd}");

    std::fs::remove_file(&html).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn md_to_pdf_renders() {
    let md = tmp("m2p.md");
    std::fs::write(&md, "# Heading\n\nbody para\n\n- alpha\n- beta\n").unwrap();

    let out = tmp("m2p.pdf");
    let r = call(
        office__md_to_pdf,
        &format!(r#"{{"input":"{md}","output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "convert: {r}");
    assert!(r["elements"].as_u64().unwrap() >= 3, "elements mapped: {r}");

    let pr = call(office__pdf_read, &format!(r#"{{"path":"{out}"}}"#));
    let text = pr["text"].as_str().unwrap();
    assert!(text.contains("Heading"), "heading in pdf: {text:?}");
    assert!(text.contains("body para"), "paragraph in pdf: {text:?}");
    assert!(text.contains("alpha"), "list item in pdf: {text:?}");

    std::fs::remove_file(&md).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn doc_add_toc_from_headings() {
    let dx = tmp("toc.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"heading","level":1,"text":"Chapter One"}},
                {{"kind":"para","text":"body a"}},
                {{"kind":"heading","level":2,"text":"Section 1.1"}},
                {{"kind":"heading","level":1,"text":"Chapter Two"}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let r = call(office__doc_add_toc, &format!(r#"{{"path":"{dx}"}}"#));
    assert_eq!(r["entries"], 3, "three headings in toc: {r}");

    // the TOC heading is now the document's first heading
    let ol = call(office__doc_outline, &format!(r#"{{"path":"{dx}"}}"#));
    assert_eq!(
        ol["outline"][0]["text"], "Table of Contents",
        "toc title first: {ol}"
    );
    // TOC entry paragraphs include the chapter titles
    let rd = call(office__doc_read, &format!(r#"{{"path":"{dx}"}}"#));
    let joined = rd["paragraphs"].to_string();
    assert!(
        joined.contains("Chapter One"),
        "toc lists Chapter One: {joined}"
    );
    assert!(
        joined.contains("Section 1.1"),
        "toc lists subsection: {joined}"
    );

    std::fs::remove_file(&dx).ok();
}

#[test]
fn doc_write_footer_page_numbers() {
    use std::io::Read as _;
    let dx = tmp("pgnum.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[{{"kind":"para","text":"body"}}],"footer":"Confidential","page_numbers":true}}"#
        ),
    );
    assert_eq!(w["ok"], true, "doc write: {w}");

    // inspect the generated footer part for the footer text + PAGE field
    let bytes = std::fs::read(&dx).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut footer = String::new();
    // footer part name is footer1.xml / footer2.xml depending on writer
    let names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .collect();
    let fname = names
        .iter()
        .find(|n| n.starts_with("word/footer") && n.ends_with(".xml"))
        .expect("a footer part exists");
    zip.by_name(fname)
        .unwrap()
        .read_to_string(&mut footer)
        .unwrap();
    assert!(
        footer.contains("Confidential"),
        "footer text present: {footer}"
    );
    assert!(
        footer.contains("PAGE"),
        "page-number field present: {footer}"
    );

    std::fs::remove_file(&dx).ok();
}

#[test]
fn doc_to_html_structured() {
    let dx = tmp("tohtml.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"heading","level":1,"text":"Report"}},
                {{"kind":"para","text":"body text"}},
                {{"kind":"table","rows":[["a","b"],["1","2"]]}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let html = tmp("tohtml.html");
    let r = call(
        office__doc_to_html,
        &format!(r#"{{"path":"{dx}","output":"{html}"}}"#),
    );
    assert_eq!(r["ok"], true, "doc_to_html: {r}");
    let text = std::fs::read_to_string(&html).unwrap();
    assert!(text.contains("<h1>Report</h1>"), "heading html: {text}");
    assert!(text.contains("body text"), "paragraph: {text}");
    assert!(
        text.contains("<td>a</td>") && text.contains("<td>2</td>"),
        "table cells: {text}"
    );

    std::fs::remove_file(&dx).ok();
    std::fs::remove_file(&html).ok();
}

#[test]
fn doc_diff_paragraph_lcs() {
    // A: P1, P2, P3   B: P1, P2-changed, P3, P4
    let a = tmp("diff_a.docx");
    let wa = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{a}","blocks":[
                {{"kind":"para","text":"alpha"}},
                {{"kind":"para","text":"beta"}},
                {{"kind":"para","text":"gamma"}}
            ]}}"#
        ),
    );
    assert_eq!(wa["ok"], true, "write a: {wa}");
    let b = tmp("diff_b.docx");
    let wb = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{b}","blocks":[
                {{"kind":"para","text":"alpha"}},
                {{"kind":"para","text":"beta-edited"}},
                {{"kind":"para","text":"gamma"}},
                {{"kind":"para","text":"delta"}}
            ]}}"#
        ),
    );
    assert_eq!(wb["ok"], true, "write b: {wb}");

    let r = call(office__doc_diff, &format!(r#"{{"a":"{a}","b":"{b}"}}"#));
    // alpha + gamma are common; beta removed; beta-edited + delta added.
    assert_eq!(r["same"], 2, "two common paragraphs: {r}");
    assert_eq!(r["removed"], 1, "one removed: {r}");
    assert_eq!(r["added"], 2, "two added: {r}");
    assert_eq!(r["removed_paragraphs"][0], "beta", "removed text: {r}");
    let added: Vec<String> = r["added_paragraphs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        added.contains(&"beta-edited".to_string()),
        "added beta-edited: {r}"
    );
    assert!(added.contains(&"delta".to_string()), "added delta: {r}");

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[test]
fn doc_comments_extracted_from_docx() {
    // doc_write doesn't emit comments, so hand-build a minimal docx zip with a
    // word/comments.xml part.
    let comments_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:comments xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:comment w:id="1" w:author="Jane Doe" w:date="2026-01-01T00:00:00Z" w:initials="JD">
    <w:p><w:r><w:t>Please revise this paragraph.</w:t></w:r></w:p>
  </w:comment>
  <w:comment w:id="2" w:author="Bob" w:date="2026-01-02T00:00:00Z" w:initials="B">
    <w:p><w:r><w:t>Looks good.</w:t></w:r></w:p>
  </w:comment>
</w:comments>"#;
    let zip = write_zip_entries(&[(
        "word/comments.xml".to_string(),
        comments_xml.as_bytes().to_vec(),
    )])
    .unwrap();
    let path = tmp("comments.docx");
    std::fs::write(&path, &zip).unwrap();

    let r = call(office__doc_comments, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["count"], 2, "two comments: {r}");
    let c = r["comments"].as_array().unwrap();
    assert_eq!(c[0]["author"], "Jane Doe", "author: {r}");
    assert_eq!(c[0]["id"], "1", "id: {r}");
    assert_eq!(c[0]["initials"], "JD", "initials: {r}");
    assert_eq!(c[0]["text"], "Please revise this paragraph.", "text: {r}");
    assert_eq!(c[1]["author"], "Bob", "second author: {r}");

    // a docx with no comments part returns an empty list, not an error
    let plain = tmp("nocomments.docx");
    let w = call(
        office__doc_write,
        &format!(r#"{{"path":"{plain}","blocks":[{{"kind":"para","text":"hi"}}]}}"#),
    );
    assert_eq!(w["ok"], true, "plain write: {w}");
    let r2 = call(office__doc_comments, &format!(r#"{{"path":"{plain}"}}"#));
    assert_eq!(r2["count"], 0, "no comments: {r2}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&plain).ok();
}

#[test]
fn doc_footnotes_extracted_from_docx() {
    // Real footnote (id 1) plus the two boilerplate separators (ids -1, 0).
    let footnotes_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:t>sep</w:t></w:r></w:p></w:footnote>
  <w:footnote w:type="continuationSeparator" w:id="0"><w:p><w:r><w:t>cont</w:t></w:r></w:p></w:footnote>
  <w:footnote w:id="1"><w:p><w:r><w:t>See appendix A for details.</w:t></w:r></w:p></w:footnote>
</w:footnotes>"#;
    let zip = write_zip_entries(&[(
        "word/footnotes.xml".to_string(),
        footnotes_xml.as_bytes().to_vec(),
    )])
    .unwrap();
    let path = tmp("footnotes.docx");
    std::fs::write(&path, &zip).unwrap();

    let r = call(office__doc_footnotes, &format!(r#"{{"path":"{path}"}}"#));
    // separators skipped → only the real footnote remains
    assert_eq!(r["count"], 1, "one real footnote: {r}");
    assert_eq!(r["notes"][0]["id"], "1", "footnote id: {r}");
    assert_eq!(
        r["notes"][0]["text"], "See appendix A for details.",
        "footnote text: {r}"
    );

    // a docx without the footnotes part returns empty
    let plain = tmp("nofn.docx");
    let w = call(
        office__doc_write,
        &format!(r#"{{"path":"{plain}","blocks":[{{"kind":"para","text":"hi"}}]}}"#),
    );
    assert_eq!(w["ok"], true, "plain write: {w}");
    let r2 = call(office__doc_footnotes, &format!(r#"{{"path":"{plain}"}}"#));
    assert_eq!(r2["count"], 0, "no footnotes: {r2}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&plain).ok();
}

#[test]
fn doc_to_md_structured() {
    let dx = tmp("tomd.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"heading","level":1,"text":"Report"}},
                {{"kind":"para","text":"body text"}},
                {{"kind":"table","rows":[["a","b"],["1","2"]]}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let md = tmp("tomd.md");
    let r = call(
        office__doc_to_md,
        &format!(r#"{{"path":"{dx}","output":"{md}"}}"#),
    );
    assert_eq!(r["ok"], true, "doc_to_md: {r}");
    let text = std::fs::read_to_string(&md).unwrap();
    assert!(text.contains("# Report"), "heading markdown: {text}");
    assert!(text.contains("body text"), "paragraph: {text}");
    // table cells must be non-empty (the block_plain_text fix)
    assert!(text.contains("| a | b |"), "table header row: {text}");
    assert!(text.contains("| 1 | 2 |"), "table data row: {text}");

    std::fs::remove_file(&dx).ok();
    std::fs::remove_file(&md).ok();
}

#[test]
fn html_to_doc_structured() {
    let html = tmp("in.html");
    std::fs::write(
        &html,
        "<html><body>\n<h2>Sec</h2>\n<p>txt body</p>\n<ul><li>one</li><li>two</li></ul>\n<table><tr><td>a</td><td>b</td></tr><tr><td>1</td><td>2</td></tr></table>\n</body></html>",
    )
    .unwrap();

    let out = tmp("html_out.docx");
    let r = call(
        office__html_to_doc,
        &format!(r#"{{"input":"{html}","output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "html_to_doc: {r}");

    let ol = call(office__doc_outline, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(ol["count"], 1, "one heading: {ol}");
    assert_eq!(ol["outline"][0]["level"], 2, "h2 level: {ol}");
    assert_eq!(ol["outline"][0]["text"], "Sec", "heading text: {ol}");

    let tb = call(office__doc_tables, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(tb["count"], 1, "one table: {tb}");
    assert_eq!(tb["tables"][0]["rows"][1][1], "2", "table cell: {tb}");

    let rd = call(office__doc_read, &format!(r#"{{"path":"{out}"}}"#));
    let joined = rd["paragraphs"].to_string();
    assert!(joined.contains("txt body"), "paragraph: {joined}");
    assert!(
        joined.contains("one") && joined.contains("two"),
        "list items: {joined}"
    );

    std::fs::remove_file(&html).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn html_to_text_strips() {
    let html = tmp("strip.html");
    std::fs::write(
        &html,
        "<html><head><style>p{color:red}</style></head><body><h1>Title</h1><p>Hello &amp; welcome</p><script>ignore()</script><p>Line&nbsp;two</p></body></html>",
    )
    .unwrap();
    let r = call(office__html_to_text, &format!(r#"{{"input":"{html}"}}"#));
    let t = r["text"].as_str().unwrap();
    assert!(t.contains("Title"), "heading text: {t:?}");
    assert!(t.contains("Hello & welcome"), "entity decoded: {t:?}");
    assert!(t.contains("Line two"), "nbsp -> space: {t:?}");
    assert!(!t.contains("ignore"), "script dropped: {t:?}");
    assert!(!t.contains("color:red"), "style dropped: {t:?}");
    assert!(!t.contains('<'), "no tags remain: {t:?}");

    std::fs::remove_file(&html).ok();
}

#[test]
fn md_to_doc_structured() {
    let md = tmp("in.md");
    std::fs::write(
        &md,
        "# Title\n\nintro paragraph\n\n- one\n- two\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n",
    )
    .unwrap();

    let out = tmp("md_out.docx");
    let r = call(
        office__md_to_doc,
        &format!(r#"{{"input":"{md}","output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "md_to_doc: {r}");

    // heading preserved
    let ol = call(office__doc_outline, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(ol["count"], 1, "one heading: {ol}");
    assert_eq!(ol["outline"][0]["text"], "Title", "heading text: {ol}");
    assert_eq!(ol["outline"][0]["level"], 1, "heading level: {ol}");

    // table preserved
    let tb = call(office__doc_tables, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(tb["count"], 1, "one table: {tb}");
    assert_eq!(tb["tables"][0]["rows"][0][0], "a", "table header: {tb}");
    assert_eq!(tb["tables"][0]["rows"][1][1], "2", "table cell: {tb}");

    // list items + paragraph present in text
    let rd = call(office__doc_read, &format!(r#"{{"path":"{out}"}}"#));
    let joined = rd["paragraphs"].to_string();
    assert!(joined.contains("intro paragraph"), "paragraph: {joined}");
    assert!(
        joined.contains("one") && joined.contains("two"),
        "list items: {joined}"
    );

    std::fs::remove_file(&md).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn doc_split_at_headings() {
    let path = tmp("split.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{path}","blocks":[
                {{"kind":"heading","level":1,"text":"Chapter 1"}},
                {{"kind":"para","text":"alpha"}},
                {{"kind":"heading","level":1,"text":"Chapter 2"}},
                {{"kind":"para","text":"bravo"}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let dir = tmp("split_out");
    std::fs::create_dir_all(&dir).unwrap();
    let r = call(
        office__doc_split,
        &format!(r#"{{"path":"{path}","dir":"{dir}","prefix":"ch"}}"#),
    );
    assert_eq!(r["count"], 2, "two sections: {r}");
    let s1 = call(
        office__doc_read,
        &format!(r#"{{"path":"{dir}/ch-1.docx"}}"#),
    );
    let j1 = s1["paragraphs"].to_string();
    assert!(
        j1.contains("Chapter 1") && j1.contains("alpha"),
        "section 1 content: {j1}"
    );
    assert!(!j1.contains("Chapter 2"), "section 1 excludes ch2: {j1}");
    let s2 = call(
        office__doc_read,
        &format!(r#"{{"path":"{dir}/ch-2.docx"}}"#),
    );
    assert!(
        s2["paragraphs"].to_string().contains("bravo"),
        "section 2 content: {s2}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn doc_append_blocks() {
    let path = tmp("ap.docx");
    let w = call(
        office__doc_write,
        &format!(r#"{{"path":"{path}","blocks":[{{"kind":"para","text":"first"}}]}}"#),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("ap_out.docx");
    let r = call(
        office__doc_append,
        &format!(
            r#"{{"path":"{path}","blocks":[{{"kind":"para","text":"second"}}],"output":"{out}"}}"#
        ),
    );
    assert_eq!(r["added"], 1, "added 1 block: {r}");
    assert_eq!(r["blocks"], 2, "2 blocks total: {r}");
    let rd = call(office__doc_read, &format!(r#"{{"path":"{out}"}}"#));
    let joined = rd["paragraphs"].to_string();
    assert!(
        joined.contains("first") && joined.contains("second"),
        "both paras: {joined}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn doc_wordfreq_ranks_words() {
    let path = tmp("wf.txt");
    std::fs::write(&path, "the cat sat on the mat the cat ran").unwrap();

    // default: no stopwords, ignore_case
    let r = call(office__doc_wordfreq, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["total"], 9, "9 tokens: {r}");
    assert_eq!(r["unique"], 6, "6 unique: {r}");
    assert_eq!(r["words"][0]["word"], "the", "top word: {r}");
    assert_eq!(r["words"][0]["count"], 3, "the x3: {r}");
    assert_eq!(r["words"][1]["word"], "cat", "second: {r}");
    assert_eq!(r["words"][1]["count"], 2, "cat x2: {r}");

    // with stopwords: "the"/"on" filtered -> cat leads
    let s = call(
        office__doc_wordfreq,
        &format!(r#"{{"path":"{path}","stopwords":true}}"#),
    );
    assert_eq!(s["words"][0]["word"], "cat", "stopword-filtered top: {s}");
    assert_eq!(s["total"], 5, "5 non-stopword tokens: {s}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn doc_readability_flesch() {
    let path = tmp("read.txt");
    // 6 one-syllable words, 2 sentences → wps=3, spw=1.
    std::fs::write(&path, "The cat sat. The dog ran.").unwrap();

    let r = call(office__doc_readability, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["words"], 6, "word count: {r}");
    assert_eq!(r["sentences"], 2, "sentence count: {r}");
    assert_eq!(r["syllables"], 6, "syllable count: {r}");
    // ease = 206.835 - 1.015*3 - 84.6*1 = 119.19
    assert!(
        (r["flesch_reading_ease"].as_f64().unwrap() - 119.19).abs() < 0.01,
        "reading ease: {r}"
    );
    // grade = 0.39*3 + 11.8*1 - 15.59 = -2.62
    assert!(
        (r["flesch_kincaid_grade"].as_f64().unwrap() + 2.62).abs() < 0.01,
        "grade level: {r}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn doc_sentences_segments() {
    let path = tmp("sent.txt");
    std::fs::write(
        &path,
        "First sentence here. Second one!  Third? And a trailing tail",
    )
    .unwrap();

    let r = call(office__doc_sentences, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["count"], 4, "four sentences incl trailing tail: {r}");
    assert_eq!(
        r["sentences"][0], "First sentence here.",
        "first sentence: {r}"
    );
    assert_eq!(r["sentences"][1], "Second one!", "second sentence: {r}");
    assert_eq!(
        r["sentences"][3], "And a trailing tail",
        "trailing no-terminator: {r}"
    );

    // max caps the count
    let rm = call(
        office__doc_sentences,
        &format!(r#"{{"path":"{path}","max":2}}"#),
    );
    assert_eq!(rm["count"], 2, "max caps to 2: {rm}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn doc_summary_extractive() {
    let path = tmp("summary.txt");
    // The "budget" sentence carries the most repeated content words.
    std::fs::write(
        &path,
        "The weather today is mild. The budget report shows budget growth and budget savings across the budget. \
         Lunch was fine. Someone left early.",
    )
    .unwrap();

    let r = call(
        office__doc_summary,
        &format!(r#"{{"path":"{path}","sentences":1}}"#),
    );
    assert_eq!(r["count"].as_u64().unwrap(), 1, "one sentence kept: {r}");
    assert_eq!(r["total_sentences"].as_u64().unwrap(), 4, "four total: {r}");
    let top = r["summary"][0].as_str().unwrap();
    assert!(top.contains("budget"), "picks the topical sentence: {r}");

    // asking for more than available returns all, in order
    let all = call(
        office__doc_summary,
        &format!(r#"{{"path":"{path}","sentences":10}}"#),
    );
    assert_eq!(
        all["count"].as_u64().unwrap(),
        4,
        "all returned when fewer exist: {all}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn doc_extract_matches() {
    let path = tmp("extract.txt");
    std::fs::write(
        &path,
        "Reach a@x.com or b@y.org. Duplicate a@x.com again. Visit https://example.com too.",
    )
    .unwrap();

    // email preset, unique by default -> 2 distinct
    let e = call(
        office__doc_extract,
        &format!(r#"{{"path":"{path}","preset":"email"}}"#),
    );
    assert_eq!(e["count"].as_u64().unwrap(), 2, "two distinct emails: {e}");
    let ms: Vec<&str> = e["matches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        ms.contains(&"a@x.com") && ms.contains(&"b@y.org"),
        "emails: {e}"
    );

    // url preset
    let u = call(
        office__doc_extract,
        &format!(r#"{{"path":"{path}","preset":"url"}}"#),
    );
    assert_eq!(u["matches"][0], "https://example.com", "url extracted: {u}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn doc_find_and_slides_find() {
    // doc_find over a docx with three paragraphs
    let dx = tmp("find.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"para","text":"Hello world"}},
                {{"kind":"para","text":"the world turns"}},
                {{"kind":"para","text":"goodbye"}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "doc write: {w}");
    let f = call(
        office__doc_find,
        &format!(r#"{{"path":"{dx}","query":"world","ignore_case":true}}"#),
    );
    assert_eq!(f["count"], 2, "two world hits: {f}");
    let paras: Vec<u64> = f["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["paragraph"].as_u64().unwrap())
        .collect();
    assert_eq!(paras, vec![1, 2], "paragraphs 1 and 2: {f}");

    // regex mode: paragraphs starting with "the" or "good"
    let fr = call(
        office__doc_find,
        &format!(r#"{{"path":"{dx}","query":"^(the|good)","regex":true}}"#),
    );
    assert_eq!(
        fr["count"], 2,
        "regex matches 'the world' and 'goodbye': {fr}"
    );

    // slides_find over a written pptx deck
    let px = tmp("find.pptx");
    let sw = call(
        office__slides_write,
        &format!(r#"{{"path":"{px}","slides":[{{"title":"Intro","body":["alpha","beta"]}}]}}"#),
    );
    assert_eq!(sw["ok"], true, "slides write: {sw}");
    let sf = call(
        office__slides_find,
        &format!(r#"{{"path":"{px}","query":"beta"}}"#),
    );
    assert_eq!(sf["count"], 1, "one beta hit: {sf}");
    assert_eq!(sf["matches"][0]["slide"], 1, "slide 1: {sf}");
    assert_eq!(sf["matches"][0]["where"], "text", "in text: {sf}");

    std::fs::remove_file(&dx).ok();
    std::fs::remove_file(&px).ok();
}

#[test]
fn doc_outline_headings() {
    let path = tmp("outline.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{path}","blocks":[
                {{"kind":"heading","level":1,"text":"A"}},
                {{"kind":"para","text":"x"}},
                {{"kind":"heading","level":2,"text":"B"}},
                {{"kind":"heading","level":1,"text":"C"}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");
    let o = call(office__doc_outline, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(o["count"], 3, "3 headings: {o}");
    assert_eq!(o["outline"][0]["level"], 1, "A level: {o}");
    assert_eq!(o["outline"][0]["text"], "A");
    assert_eq!(o["outline"][1]["level"], 2, "B level: {o}");
    assert_eq!(o["outline"][1]["text"], "B");
    assert_eq!(o["outline"][2]["text"], "C", "C present: {o}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn doc_blocks_ordered_structural_read() {
    // docx: write headings/paras/table in order, recover the block sequence
    let dx = tmp("blocks.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"heading","level":1,"text":"Title"}},
                {{"kind":"para","text":"Body text."}},
                {{"kind":"table","rows":[["A","B"],["1","2"]]}},
                {{"kind":"heading","level":2,"text":"Sub"}},
                {{"kind":"para","text":"End."}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "docx write: {w}");
    let b = call(office__doc_blocks, &format!(r#"{{"path":"{dx}"}}"#));
    let bl = b["blocks"].as_array().expect("blocks array");
    assert_eq!(b["count"], 5, "five blocks in order: {b}");
    assert_eq!(bl[0]["kind"], "heading");
    assert_eq!(bl[0]["level"], 1);
    assert_eq!(bl[0]["text"], "Title");
    assert_eq!(bl[1]["kind"], "para");
    assert_eq!(bl[1]["text"], "Body text.");
    assert_eq!(bl[2]["kind"], "table");
    assert_eq!(bl[2]["rows"][1][0], "1", "table cell in order: {b}");
    assert_eq!(bl[3]["kind"], "heading");
    assert_eq!(bl[3]["level"], 2);
    assert_eq!(bl[4]["text"], "End.");

    // odt: hand-built content.xml with heading / para / table in order
    let od = tmp("blocks.odt");
    {
        use std::io::Write as _;
        use zip::write::SimpleFileOptions;
        let f = std::fs::File::create(&od).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opt = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zw.start_file("content.xml", opt).unwrap();
        zw.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
  xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
 <office:body><office:text>
  <text:h text:outline-level="1">Heading One</text:h>
  <text:p>A paragraph.</text:p>
  <table:table>
   <table:table-row>
    <table:table-cell><text:p>x</text:p></table:table-cell>
    <table:table-cell><text:p>y</text:p></table:table-cell>
   </table:table-row>
  </table:table>
 </office:text></office:body>
</office:document-content>"#,
        )
        .unwrap();
        zw.finish().unwrap();
    }
    let ob = call(office__doc_blocks, &format!(r#"{{"path":"{od}"}}"#));
    let obl = ob["blocks"].as_array().expect("odt blocks array");
    assert_eq!(ob["count"], 3, "odt three blocks: {ob}");
    assert_eq!(obl[0]["kind"], "heading");
    assert_eq!(obl[0]["level"], 1);
    assert_eq!(obl[0]["text"], "Heading One");
    assert_eq!(obl[1]["kind"], "para");
    assert_eq!(obl[1]["text"], "A paragraph.");
    assert_eq!(obl[2]["kind"], "table");
    assert_eq!(obl[2]["rows"][0][1], "y", "odt table cell: {ob}");

    std::fs::remove_file(&dx).ok();
    std::fs::remove_file(&od).ok();
}

#[test]
fn slides_read_extracts_speaker_notes() {
    use std::io::Write as _;
    use zip::write::SimpleFileOptions;
    let opt = || SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    // pptx: slide1 + its rels pointing at notesSlide1
    let px = tmp("notes.pptx");
    {
        let mut zw = zip::ZipWriter::new(std::fs::File::create(&px).unwrap());
        zw.start_file("ppt/slides/slide1.xml", opt()).unwrap();
        zw.write_all(
            br#"<p:sld xmlns:p="p" xmlns:a="a"><a:p><a:r><a:t>Slide Title</a:t></a:r></a:p></p:sld>"#,
        )
        .unwrap();
        zw.start_file("ppt/slides/_rels/slide1.xml.rels", opt())
            .unwrap();
        zw.write_all(
            br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/>
</Relationships>"#,
        )
        .unwrap();
        zw.start_file("ppt/notesSlides/notesSlide1.xml", opt())
            .unwrap();
        zw.write_all(
            br#"<p:notes xmlns:p="p" xmlns:a="a"><a:p><a:r><a:t>Remember to smile</a:t></a:r></a:p></p:notes>"#,
        )
        .unwrap();
        zw.finish().unwrap();
    }
    let ps = call(office__slides_read, &format!(r#"{{"path":"{px}"}}"#));
    assert_eq!(
        ps["slides"][0]["text"][0], "Slide Title",
        "pptx slide text: {ps}"
    );
    assert_eq!(
        ps["slides"][0]["notes"][0], "Remember to smile",
        "pptx speaker notes: {ps}"
    );

    // odp: a draw:page with body text + a presentation:notes subtree
    let ox = tmp("notes.odp");
    {
        let mut zw = zip::ZipWriter::new(std::fs::File::create(&ox).unwrap());
        zw.start_file("content.xml", opt()).unwrap();
        zw.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
  xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0"
  xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
 <office:body><office:presentation>
  <draw:page>
   <draw:frame><draw:text-box><text:p>Slide Body</text:p></draw:text-box></draw:frame>
   <presentation:notes><draw:frame><draw:text-box><text:p>Speaker note here</text:p></draw:text-box></draw:frame></presentation:notes>
  </draw:page>
 </office:presentation></office:body>
</office:document-content>"#,
        )
        .unwrap();
        zw.finish().unwrap();
    }
    let os = call(office__slides_read, &format!(r#"{{"path":"{ox}"}}"#));
    assert_eq!(
        os["slides"][0]["text"],
        json!(["Slide Body"]),
        "odp slide text only: {os}"
    );
    assert_eq!(
        os["slides"][0]["notes"],
        json!(["Speaker note here"]),
        "odp notes separated: {os}"
    );

    std::fs::remove_file(&px).ok();
    std::fs::remove_file(&ox).ok();
}

#[test]
fn doc_links_extracted_from_docx_and_odt() {
    // docx: write a hyperlink block, recover {text,url} via the rels map
    let dx = tmp("links.docx");
    let w = call(
        office__doc_write,
        &format!(
            r#"{{"path":"{dx}","blocks":[
                {{"kind":"para","text":"see"}},
                {{"kind":"link","url":"https://example.com/path","text":"Example"}}
            ]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "docx write: {w}");
    let l = call(office__doc_links, &format!(r#"{{"path":"{dx}"}}"#));
    assert_eq!(l["count"], 1, "one link: {l}");
    assert_eq!(l["links"][0]["text"], "Example", "link text: {l}");
    assert_eq!(
        l["links"][0]["url"], "https://example.com/path",
        "link url: {l}"
    );

    // odt: hand-built content.xml with a text:a whose href contains an entity
    let od = tmp("links.odt");
    {
        use std::io::Write as _;
        use zip::write::SimpleFileOptions;
        let f = std::fs::File::create(&od).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opt = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zw.start_file("content.xml", opt).unwrap();
        zw.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
  xmlns:xlink="http://www.w3.org/1999/xlink">
 <office:body><office:text>
  <text:p>go <text:a xlink:href="https://ex.com/?a=1&amp;b=2">here</text:a></text:p>
 </office:text></office:body>
</office:document-content>"#,
        )
        .unwrap();
        zw.finish().unwrap();
    }
    let ol = call(office__doc_links, &format!(r#"{{"path":"{od}"}}"#));
    assert_eq!(ol["count"], 1, "odt one link: {ol}");
    assert_eq!(ol["links"][0]["text"], "here", "odt link text: {ol}");
    assert_eq!(
        ol["links"][0]["url"], "https://ex.com/?a=1&b=2",
        "odt href unescaped: {ol}"
    );

    std::fs::remove_file(&dx).ok();
    std::fs::remove_file(&od).ok();
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
fn image_scale_by_factor() {
    let n = call(
        office__img_new,
        r#"{"width":40,"height":20,"color":[1,2,3,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    // halve
    let r = call(
        office__img_scale,
        &format!(r#"{{"handle":{h},"factor":0.5}}"#),
    );
    assert_eq!(r["width"], 20, "half width: {r}");
    assert_eq!(r["height"], 10, "half height: {r}");
    // double (from the now-20x10)
    let r2 = call(
        office__img_scale,
        &format!(r#"{{"handle":{h},"factor":2.0}}"#),
    );
    assert_eq!(r2["width"], 40, "doubled width: {r2}");
    assert_eq!(r2["height"], 20, "doubled height: {r2}");
    // a non-positive factor errors
    let bad = call(
        office__img_scale,
        &format!(r#"{{"handle":{h},"factor":0}}"#),
    );
    assert!(bad["error"].is_string(), "zero factor rejected: {bad}");

    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
}

#[test]
fn image_fit_letterbox() {
    // 100x40 (wide) fit into a 50x50 box → scaled to 50x20, centered.
    let n = call(
        office__img_new,
        r#"{"width":100,"height":40,"color":[200,100,50,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    let r = call(
        office__img_fit,
        &format!(r#"{{"handle":{h},"width":50,"height":50,"color":[0,0,0,255]}}"#),
    );
    assert_eq!(r["width"], 50, "exact canvas width: {r}");
    assert_eq!(r["height"], 50, "exact canvas height: {r}");

    // center row should hold the scaled image; top row is letterbox background.
    let top = call(
        office__img_get_pixel,
        &format!(r#"{{"handle":{h},"x":25,"y":0}}"#),
    );
    assert_eq!(top["r"].as_u64().unwrap(), 0, "top is background: {top}");
    let mid = call(
        office__img_get_pixel,
        &format!(r#"{{"handle":{h},"x":25,"y":25}}"#),
    );
    assert_eq!(
        mid["r"].as_u64().unwrap(),
        200,
        "center is the image: {mid}"
    );

    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
}

#[test]
fn image_concat_edge_to_edge() {
    // 40x20 + 30x10, horizontal -> 70 wide, 20 tall (max), flush (no padding)
    let a = call(
        office__img_new,
        r#"{"width":40,"height":20,"color":[255,0,0,255]}"#,
    );
    let b = call(
        office__img_new,
        r#"{"width":30,"height":10,"color":[0,0,255,255]}"#,
    );
    let ha = a["handle"].as_u64().unwrap();
    let hb = b["handle"].as_u64().unwrap();

    let r = call(
        office__img_concat,
        &format!(r#"{{"handles":[{ha},{hb}],"axis":"h"}}"#),
    );
    assert_eq!(r["width"], 70, "summed widths: {r}");
    assert_eq!(r["height"], 20, "max height: {r}");

    // vertical -> 40 wide (max), 30 tall (sum)
    let rv = call(
        office__img_concat,
        &format!(r#"{{"handles":[{ha},{hb}],"axis":"v"}}"#),
    );
    assert_eq!(rv["width"], 40, "max width: {rv}");
    assert_eq!(rv["height"], 30, "summed heights: {rv}");

    for h in [
        ha,
        hb,
        r["handle"].as_u64().unwrap(),
        rv["handle"].as_u64().unwrap(),
    ] {
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    }
}

#[test]
fn image_canvas_resize_anchor() {
    // 20x10 image onto a 40x40 canvas, top-left anchor
    let n = call(
        office__img_new,
        r#"{"width":20,"height":10,"color":[200,50,50,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    let r = call(
        office__img_canvas,
        &format!(
            r#"{{"handle":{h},"width":40,"height":40,"anchor":"topleft","color":[0,0,0,255]}}"#
        ),
    );
    assert_eq!(r["width"], 40, "canvas width: {r}");
    assert_eq!(r["height"], 40, "canvas height: {r}");
    // top-left pixel is the image (200,50,50); bottom-right is background (0,0,0)
    let tl = call(
        office__img_get_pixel,
        &format!(r#"{{"handle":{h},"x":0,"y":0}}"#),
    );
    assert_eq!(
        tl["r"].as_u64().unwrap(),
        200,
        "top-left is the image: {tl}"
    );
    let br = call(
        office__img_get_pixel,
        &format!(r#"{{"handle":{h},"x":39,"y":39}}"#),
    );
    assert_eq!(
        br["r"].as_u64().unwrap(),
        0,
        "bottom-right is background: {br}"
    );

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
fn chart_from_sheet_renders() {
    let path = tmp("cfs.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["month","sales","cost"],["Jan",10,5],["Feb",20,8]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    // SVG output, categories from the month column, series auto-detected
    let svg = tmp("cfs.svg");
    let r = call(
        office__chart_from_sheet,
        &format!(
            r#"{{"path":"{path}","output":"{svg}","type":"bar","categories":"month","title":"Sales"}}"#
        ),
    );
    assert_eq!(r["ok"], true, "chart_from_sheet svg: {r}");
    assert_eq!(r["format"], "svg", "svg format: {r}");
    let content = std::fs::read_to_string(&svg).unwrap();
    assert!(
        content.contains("<svg"),
        "is svg: {}",
        &content[..content.len().min(40)]
    );

    // PNG output too
    let png = tmp("cfs.png");
    let rp = call(
        office__chart_from_sheet,
        &format!(r#"{{"path":"{path}","output":"{png}","type":"line","categories":"month"}}"#),
    );
    assert_eq!(rp["ok"], true, "chart_from_sheet png: {rp}");
    assert!(
        std::fs::metadata(&png)
            .map(|m| m.len() > 0)
            .unwrap_or(false),
        "png written"
    );

    for f in [&path, &svg, &png] {
        std::fs::remove_file(f).ok();
    }
}

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

    let path = tmp("build.pdf");
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
fn images_to_pdf_one_per_page() {
    // two PNGs of different sizes
    let p1 = tmp("im1.png");
    let p2 = tmp("im2.png");
    let h1 = call(
        office__img_new,
        r#"{"width":64,"height":48,"color":[200,0,0,255]}"#,
    );
    let h2 = call(
        office__img_new,
        r#"{"width":40,"height":90,"color":[0,0,200,255]}"#,
    );
    call(
        office__img_save,
        &format!(
            r#"{{"handle":{},"path":"{p1}"}}"#,
            h1["handle"].as_u64().unwrap()
        ),
    );
    call(
        office__img_save,
        &format!(
            r#"{{"handle":{},"path":"{p2}"}}"#,
            h2["handle"].as_u64().unwrap()
        ),
    );

    let out = tmp("album.pdf");
    let r = call(
        office__images_to_pdf,
        &format!(r#"{{"images":["{p1}","{p2}"],"output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "images_to_pdf: {r}");
    assert_eq!(r["pages"], 2, "one page per image: {r}");
    let info = call(office__pdf_info, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(info["pages"], 2, "pdf has 2 pages: {info}");

    for f in [&p1, &p2, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn sheet_to_pdf_table() {
    let path = tmp("s2p.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["name","qty"],["widget",3],["gadget",7]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let out = tmp("s2p.pdf");
    let r = call(
        office__sheet_to_pdf,
        &format!(r#"{{"path":"{path}","output":"{out}","title":"Inventory"}}"#),
    );
    assert_eq!(r["ok"], true, "sheet_to_pdf: {r}");
    assert!(r["pages"].as_u64().unwrap_or(0) >= 1, "has pages: {r}");
    // the rendered PDF re-reads and contains the title + cell text
    let rd = call(office__pdf_read, &format!(r#"{{"path":"{out}"}}"#));
    let txt = rd["text"].as_str().unwrap_or("");
    assert!(txt.contains("Inventory"), "title rendered: {txt:?}");
    assert!(
        txt.contains("widget") && txt.contains("gadget"),
        "cells rendered: {txt:?}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
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
fn pdf_blank_generates_pages() {
    // 3-page A4 blank
    let out = tmp("blank.pdf");
    let r = call(
        office__pdf_blank,
        &format!(r#"{{"output":"{out}","pages":3}}"#),
    );
    assert_eq!(r["pages"], 3, "three pages: {r}");
    assert!(
        (r["width"].as_f64().unwrap() - 595.0).abs() < 1e-6,
        "A4 width: {r}"
    );
    // re-load and confirm page count + page size
    let info = call(office__pdf_info, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(info["pages"], 3, "info confirms 3 pages: {info}");

    // letter size, single page
    let outl = tmp("blank_letter.pdf");
    let rl = call(
        office__pdf_blank,
        &format!(r#"{{"output":"{outl}","size":"letter"}}"#),
    );
    assert_eq!(rl["pages"], 1, "default 1 page: {rl}");
    assert!(
        (rl["width"].as_f64().unwrap() - 612.0).abs() < 1e-6,
        "letter width: {rl}"
    );
    let il = call(office__pdf_info, &format!(r#"{{"path":"{outl}"}}"#));
    assert_eq!(il["pages"], 1, "letter info 1 page: {il}");

    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&outl).ok();
}

#[test]
fn pdf_encrypt_decrypt_compress() {
    // a 2-page source PDF
    let src = tmp("sec.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[
                {{"type":"heading","level":1,"text":"Secret 1"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Secret 2"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    // RC4 encrypt with both passwords set so neither is empty
    let enc = tmp("sec.enc.pdf");
    let e = call(
        office__pdf_encrypt,
        &format!(
            r#"{{"path":"{src}","output":"{enc}","owner_password":"s3cret","user_password":"s3cret","permissions":["print"]}}"#
        ),
    );
    assert_eq!(e["ok"], true, "encrypt: {e}");
    assert_eq!(e["method"], "rc4-128", "rc4 method: {e}");

    // wrong password must fail
    let dec = tmp("sec.dec.pdf");
    let bad = call(
        office__pdf_decrypt,
        &format!(r#"{{"path":"{enc}","output":"{dec}","password":"nope"}}"#),
    );
    assert!(
        !err_of(&bad).is_empty(),
        "wrong password should error: {bad}"
    );

    // correct password decrypts to a plaintext, re-readable PDF
    let good = call(
        office__pdf_decrypt,
        &format!(r#"{{"path":"{enc}","output":"{dec}","password":"s3cret"}}"#),
    );
    assert_eq!(good["ok"], true, "decrypt: {good}");
    let id = call(office__pdf_info, &format!(r#"{{"path":"{dec}"}}"#));
    assert_eq!(id["pages"], 2, "decrypted keeps 2 pages: {id}");
    let rd = call(office__pdf_read, &format!(r#"{{"path":"{dec}"}}"#));
    assert!(rd["text"].is_string(), "decrypted pdf re-reads: {rd}");

    // AES-128 path
    let aes = tmp("sec.aes.pdf");
    let ea = call(
        office__pdf_encrypt,
        &format!(
            r#"{{"path":"{src}","output":"{aes}","owner_password":"s3cret","user_password":"s3cret","aes":true}}"#
        ),
    );
    assert_eq!(ea["method"], "aes-128", "aes method: {ea}");
    let aesdec = tmp("sec.aes.dec.pdf");
    let da = call(
        office__pdf_decrypt,
        &format!(r#"{{"path":"{aes}","output":"{aesdec}","password":"s3cret"}}"#),
    );
    assert_eq!(da["ok"], true, "aes decrypt: {da}");
    let ida = call(office__pdf_info, &format!(r#"{{"path":"{aesdec}"}}"#));
    assert_eq!(ida["pages"], 2, "aes decrypted keeps 2 pages: {ida}");

    // compress: output still loads with the same page count
    let comp = tmp("sec.comp.pdf");
    let c = call(
        office__pdf_compress,
        &format!(r#"{{"path":"{src}","output":"{comp}"}}"#),
    );
    assert_eq!(c["ok"], true, "compress: {c}");
    assert!(
        c["after"].as_u64().unwrap_or(0) > 0,
        "compressed has bytes: {c}"
    );
    let ic = call(office__pdf_info, &format!(r#"{{"path":"{comp}"}}"#));
    assert_eq!(ic["pages"], 2, "compressed keeps 2 pages: {ic}");

    for f in [&src, &enc, &dec, &aes, &aesdec, &comp] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_delete_and_reorder_pages() {
    let src = tmp("pg.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[
                {{"type":"heading","level":1,"text":"Alpha"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Bravo"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Charlie"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    // delete page 2 (Bravo) -> 2 pages remain, Bravo gone
    let del = tmp("pg.del.pdf");
    let d = call(
        office__pdf_delete,
        &format!(r#"{{"path":"{src}","pages":[2],"output":"{del}"}}"#),
    );
    assert_eq!(d["pages"], 2, "two pages remain: {d}");
    let rd = call(office__pdf_read, &format!(r#"{{"path":"{del}"}}"#));
    let txt = rd["text"].as_str().unwrap_or("");
    assert!(
        txt.contains("Alpha") && txt.contains("Charlie") && !txt.contains("Bravo"),
        "kept Alpha+Charlie, dropped Bravo: {txt:?}"
    );

    // reorder to [3,1,2] -> Charlie, Alpha, Bravo
    let reo = tmp("pg.reo.pdf");
    let r = call(
        office__pdf_reorder,
        &format!(r#"{{"path":"{src}","order":[3,1,2],"output":"{reo}"}}"#),
    );
    assert_eq!(r["pages"], 3, "three pages: {r}");
    let rr = call(office__pdf_read, &format!(r#"{{"path":"{reo}"}}"#));
    let rtxt = rr["text"].as_str().unwrap_or("");
    let (ia, ib, ic) = (rtxt.find("Alpha"), rtxt.find("Bravo"), rtxt.find("Charlie"));
    assert!(
        ic.is_some() && ic < ia && ia < ib,
        "page order Charlie<Alpha<Bravo: {rtxt:?}"
    );

    for f in [&src, &del, &reo] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_extract_page_spec() {
    let src = tmp("ext.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[
                {{"type":"heading","level":1,"text":"Alpha"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Bravo"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Charlie"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Delta"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    // range-spec string "1,3-4" -> Alpha, Charlie, Delta (3 pages, in order)
    let out = tmp("ext_str.pdf");
    let r = call(
        office__pdf_extract,
        &format!(r#"{{"path":"{src}","pages":"1,3-4","output":"{out}"}}"#),
    );
    assert_eq!(r["pages"], 3, "three pages extracted: {r}");
    let rd = call(office__pdf_read, &format!(r#"{{"path":"{out}"}}"#));
    let txt = rd["text"].as_str().unwrap_or("");
    let (ia, ic, id, ib) = (
        txt.find("Alpha"),
        txt.find("Charlie"),
        txt.find("Delta"),
        txt.find("Bravo"),
    );
    assert!(ib.is_none(), "Bravo excluded: {txt:?}");
    assert!(
        ia.is_some() && ia < ic && ic < id,
        "order Alpha<Charlie<Delta: {txt:?}"
    );

    // descending range "4-3" -> Delta then Charlie via array form equivalence
    let out2 = tmp("ext_desc.pdf");
    call(
        office__pdf_extract,
        &format!(r#"{{"path":"{src}","pages":"4-3","output":"{out2}"}}"#),
    );
    let rd2 = call(office__pdf_read, &format!(r#"{{"path":"{out2}"}}"#));
    let t2 = rd2["text"].as_str().unwrap_or("");
    assert!(
        t2.find("Delta") < t2.find("Charlie"),
        "descending range Delta<Charlie: {t2:?}"
    );

    // array form also works
    let out3 = tmp("ext_arr.pdf");
    let r3 = call(
        office__pdf_extract,
        &format!(r#"{{"path":"{src}","pages":[2],"output":"{out3}"}}"#),
    );
    assert_eq!(r3["pages"], 1, "array form single page: {r3}");

    for f in [&src, &out, &out2, &out3] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_search_finds_text_per_page() {
    let src = tmp("search.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[
                {{"type":"heading","level":1,"text":"Apple Banana"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Cherry"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"apple pie apple"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    // case-insensitive "apple": page 1 (1) + page 3 (2) = 3 across 2 pages
    let r = call(
        office__pdf_search,
        &format!(r#"{{"path":"{src}","query":"apple","ignore_case":true}}"#),
    );
    assert_eq!(r["matched_pages"], 2, "two pages matched: {r}");
    let pages: Vec<u64> = r["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["page"].as_u64().unwrap())
        .collect();
    assert!(
        pages.contains(&1) && pages.contains(&3),
        "pages 1 and 3: {r}"
    );
    assert!(
        r["count"].as_u64().unwrap() >= 3,
        "at least 3 occurrences: {r}"
    );

    // case-sensitive "Cherry" -> only page 2
    let c = call(
        office__pdf_search,
        &format!(r#"{{"path":"{src}","query":"Cherry"}}"#),
    );
    assert_eq!(c["matched_pages"], 1, "one page: {c}");
    assert_eq!(c["pages"][0]["page"], 2, "page 2: {c}");

    // regex mode: words starting with capital C -> Cherry on page 2
    let rx = call(
        office__pdf_search,
        &format!(r#"{{"path":"{src}","query":"C\\w+","regex":true}}"#),
    );
    assert_eq!(rx["matched_pages"], 1, "regex matches Cherry page: {rx}");
    assert_eq!(rx["pages"][0]["page"], 2, "regex page 2: {rx}");

    std::fs::remove_file(&src).ok();
}

#[test]
fn pdf_burst_one_file_per_page() {
    let src = tmp("burst.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[
                {{"type":"heading","level":1,"text":"Alpha"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Bravo"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Charlie"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    let dir = tmp("burst_out");
    std::fs::create_dir_all(&dir).unwrap();
    let r = call(
        office__pdf_burst,
        &format!(r#"{{"path":"{src}","dir":"{dir}","prefix":"pg"}}"#),
    );
    assert_eq!(r["count"], 3, "three files: {r}");

    // each output is a single page; page 2 carries Bravo
    let files = r["files"].as_array().unwrap();
    for f in files {
        let info = call(office__pdf_info, &format!(r#"{{"path":{f}}}"#));
        assert_eq!(info["pages"], 1, "single page each: {info}");
    }
    let p2 = call(office__pdf_read, &format!(r#"{{"path":"{dir}/pg-2.pdf"}}"#));
    assert!(
        p2["text"].as_str().unwrap_or("").contains("Bravo"),
        "page 2 file holds Bravo: {p2}"
    );

    std::fs::remove_file(&src).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pdf_remove_blank_pages() {
    // page1=A, page2=blank (two consecutive pagebreaks), page3=C
    let src = tmp("rmblank.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[
                {{"type":"heading","level":1,"text":"Alpha"}},
                {{"type":"pagebreak"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Charlie"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");
    let before = call(office__pdf_info, &format!(r#"{{"path":"{src}"}}"#));
    let n_before = before["pages"].as_u64().unwrap();

    let out = tmp("rmblank_out.pdf");
    let r = call(
        office__pdf_remove_blank,
        &format!(r#"{{"path":"{src}","output":"{out}"}}"#),
    );
    assert!(
        r["removed"].as_u64().unwrap() >= 1,
        "at least one blank removed: {r}"
    );
    assert_eq!(
        r["pages"].as_u64().unwrap(),
        n_before - r["removed"].as_u64().unwrap(),
        "page count drops by removed: {r}"
    );
    // text pages survive
    let rd = call(office__pdf_read, &format!(r#"{{"path":"{out}"}}"#));
    let txt = rd["text"].as_str().unwrap_or("");
    assert!(
        txt.contains("Alpha") && txt.contains("Charlie"),
        "content kept: {rd}"
    );

    std::fs::remove_file(&src).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn pdf_crop_sets_cropbox() {
    let src = tmp("crop.pdf");
    let b = call(
        office__pdf_build,
        &format!(r#"{{"path":"{src}","elements":[{{"type":"heading","level":1,"text":"X"}}]}}"#),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    // pdf_info now reports first-page geometry (A4 = 595x842)
    let info = call(office__pdf_info, &format!(r#"{{"path":"{src}"}}"#));
    assert_eq!(info["width"], 595.0, "A4 width: {info}");
    assert_eq!(info["height"], 842.0, "A4 height: {info}");

    // explicit box
    let cb = tmp("crop_box.pdf");
    let r = call(
        office__pdf_crop,
        &format!(r#"{{"path":"{src}","box":[50,50,300,400],"output":"{cb}"}}"#),
    );
    assert_eq!(r["cropped"], 1, "one page cropped: {r}");
    let ib = call(office__pdf_info, &format!(r#"{{"path":"{cb}"}}"#));
    assert_eq!(
        ib["cropbox"],
        json!([50.0, 50.0, 300.0, 400.0]),
        "cropbox: {ib}"
    );

    // margins inset from MediaBox: [10,10,10,10] -> [10,10,585,832]
    let cm = tmp("crop_m.pdf");
    let r2 = call(
        office__pdf_crop,
        &format!(r#"{{"path":"{src}","margins":[10,10,10,10],"output":"{cm}"}}"#),
    );
    assert_eq!(r2["ok"], true, "margins crop: {r2}");
    let im = call(office__pdf_info, &format!(r#"{{"path":"{cm}"}}"#));
    assert_eq!(
        im["cropbox"],
        json!([10.0, 10.0, 585.0, 832.0]),
        "margin cropbox: {im}"
    );

    for f in [&src, &cb, &cm] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_chunk_fixed_size() {
    // 5-page source
    let src = tmp("chunk.pdf");
    let mut els = String::new();
    for (i, name) in ["P1", "P2", "P3", "P4", "P5"].iter().enumerate() {
        if i > 0 {
            els.push_str(r#"{"type":"pagebreak"},"#);
        }
        els.push_str(&format!(
            r#"{{"type":"heading","level":1,"text":"{name}"}}"#
        ));
        if i < 4 {
            els.push(',');
        }
    }
    let b = call(
        office__pdf_build,
        &format!(r#"{{"path":"{src}","elements":[{els}]}}"#),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    let dir = tmp("chunk_out");
    std::fs::create_dir_all(&dir).unwrap();
    let r = call(
        office__pdf_chunk,
        &format!(r#"{{"path":"{src}","size":2,"dir":"{dir}","prefix":"c"}}"#),
    );
    assert_eq!(r["count"], 3, "5 pages / 2 -> 3 chunks: {r}");
    // chunk 1 has 2 pages, chunk 3 has the remaining 1
    let i1 = call(office__pdf_info, &format!(r#"{{"path":"{dir}/c-1.pdf"}}"#));
    assert_eq!(i1["pages"], 2, "chunk 1 = 2 pages: {i1}");
    let i3 = call(office__pdf_info, &format!(r#"{{"path":"{dir}/c-3.pdf"}}"#));
    assert_eq!(i3["pages"], 1, "chunk 3 = 1 page: {i3}");

    std::fs::remove_file(&src).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pdf_split_ranges_extracts() {
    // 5-page source with distinct page text
    let src = tmp("ranges.pdf");
    let mut els = String::new();
    for (i, name) in ["P1", "P2", "P3", "P4", "P5"].iter().enumerate() {
        if i > 0 {
            els.push_str(r#"{"type":"pagebreak"},"#);
        }
        els.push_str(&format!(
            r#"{{"type":"heading","level":1,"text":"{name}"}}"#
        ));
        if i < 4 {
            els.push(',');
        }
    }
    let b = call(
        office__pdf_build,
        &format!(r#"{{"path":"{src}","elements":[{els}]}}"#),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    let dir = tmp("ranges_out");
    std::fs::create_dir_all(&dir).unwrap();
    let r = call(
        office__pdf_split_ranges,
        &format!(r#"{{"path":"{src}","ranges":[[1,2],[4,5]],"dir":"{dir}","prefix":"r"}}"#),
    );
    assert_eq!(r["count"], 2, "two range files: {r}");
    let i1 = call(office__pdf_info, &format!(r#"{{"path":"{dir}/r-1.pdf"}}"#));
    assert_eq!(i1["pages"], 2, "range 1 = 2 pages: {i1}");
    // second range is pages 4-5
    let t2 = call(office__pdf_read, &format!(r#"{{"path":"{dir}/r-2.pdf"}}"#));
    let txt = t2["text"].as_str().unwrap_or("");
    assert!(
        txt.contains("P4") && txt.contains("P5"),
        "range 2 holds P4/P5: {txt:?}"
    );
    assert!(!txt.contains("P1"), "range 2 excludes P1: {txt:?}");

    std::fs::remove_file(&src).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pdf_to_text_file_and_pages() {
    let src = tmp("p2t.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[
                {{"type":"heading","level":1,"text":"Alpha"}},
                {{"type":"pagebreak"}},
                {{"type":"heading","level":1,"text":"Bravo"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    // whole-document text
    let out = tmp("p2t.txt");
    let r = call(
        office__pdf_to_text,
        &format!(r#"{{"path":"{src}","output":"{out}"}}"#),
    );
    assert_eq!(r["pages"], 2, "two pages: {r}");
    let txt = std::fs::read_to_string(&out).unwrap();
    assert!(
        txt.contains("Alpha") && txt.contains("Bravo"),
        "joined text: {txt:?}"
    );

    // per-page files
    let dir = tmp("p2t_dir");
    std::fs::create_dir_all(&dir).unwrap();
    let rp = call(
        office__pdf_to_text,
        &format!(r#"{{"path":"{src}","dir":"{dir}"}}"#),
    );
    assert_eq!(rp["count"], 2, "two page files: {rp}");
    let p2 = std::fs::read_to_string(format!("{dir}/page-2.txt")).unwrap();
    assert!(p2.contains("Bravo"), "page 2 text: {p2:?}");

    std::fs::remove_file(&src).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pdf_stats_counts_words_and_pages() {
    let src = tmp("pstats.pdf");
    // page 1: "Report alpha beta gamma" (4 words); page 2: "delta" (1 word)
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[
                {{"type":"heading","level":1,"text":"Report"}},
                {{"type":"paragraph","text":"alpha beta gamma"}},
                {{"type":"pagebreak"}},
                {{"type":"paragraph","text":"delta"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    let r = call(office__pdf_stats, &format!(r#"{{"path":"{src}"}}"#));
    assert_eq!(r["pages"], 2, "two pages: {r}");
    assert_eq!(r["words"], 5, "five words total: {r}");
    let per = r["per_page"].as_array().unwrap();
    assert_eq!(per.len(), 2, "per-page entries: {r}");
    assert_eq!(per[0]["page"], 1, "1-based page: {r}");
    assert_eq!(per[1]["words"], 1, "page 2 word count: {r}");
    assert!(r["chars"].as_u64().unwrap() > 0, "chars counted: {r}");
    assert!(
        r["chars_no_spaces"].as_u64().unwrap() < r["chars"].as_u64().unwrap(),
        "no-space char count is smaller: {r}"
    );

    std::fs::remove_file(&src).ok();
}

#[test]
fn pdf_add_link_then_links_round_trip() {
    let src = tmp("plink.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[
                {{"type":"paragraph","text":"page one"}},
                {{"type":"pagebreak"}},
                {{"type":"paragraph","text":"page two"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    // add a link on page 2 with an explicit rect
    let r = call(
        office__pdf_add_link,
        &format!(
            r#"{{"path":"{src}","page":2,"url":"https://example.com/x","rect":[72,700,300,720]}}"#
        ),
    );
    assert_eq!(r["ok"], true, "add_link: {r}");

    let lk = call(office__pdf_links, &format!(r#"{{"path":"{src}"}}"#));
    assert_eq!(lk["count"], 1, "one link: {lk}");
    assert_eq!(lk["links"][0]["page"], 2, "on page 2: {lk}");
    assert_eq!(
        lk["links"][0]["url"], "https://example.com/x",
        "url round-trips: {lk}"
    );
    let rect = lk["links"][0]["rect"].as_array().unwrap();
    assert_eq!(rect.len(), 4, "rect has 4 coords: {lk}");
    assert!(
        (rect[0].as_f64().unwrap() - 72.0).abs() < 1e-6,
        "rect x0: {lk}"
    );

    std::fs::remove_file(&src).ok();
}

#[test]
fn pdf_remove_annotations_strips_links() {
    let src = tmp("prm.pdf");
    let b = call(
        office__pdf_build,
        &format!(r#"{{"path":"{src}","elements":[{{"type":"paragraph","text":"page"}}]}}"#),
    );
    assert_eq!(b["ok"], true, "build: {b}");
    call(
        office__pdf_add_link,
        &format!(r#"{{"path":"{src}","page":1,"url":"https://a.test"}}"#),
    );
    assert_eq!(
        call(office__pdf_links, &format!(r#"{{"path":"{src}"}}"#))["count"],
        1,
        "link present before removal"
    );

    // subtype filter that doesn't match leaves the link intact
    let keep = tmp("prm_keep.pdf");
    call(
        office__pdf_remove_annotations,
        &format!(r#"{{"path":"{src}","subtype":"Highlight","output":"{keep}"}}"#),
    );
    assert_eq!(
        call(office__pdf_links, &format!(r#"{{"path":"{keep}"}}"#))["count"],
        1,
        "non-matching subtype keeps link"
    );

    // removing all annotations clears the link
    let out = tmp("prm_out.pdf");
    let r = call(
        office__pdf_remove_annotations,
        &format!(r#"{{"path":"{src}","output":"{out}"}}"#),
    );
    assert_eq!(r["removed"], 1, "one annotation removed: {r}");
    assert_eq!(
        call(office__pdf_links, &format!(r#"{{"path":"{out}"}}"#))["count"],
        0,
        "link gone after removal"
    );

    for f in [&src, &keep, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_highlight_adds_annotation() {
    let src = tmp("phl.pdf");
    let b = call(
        office__pdf_build,
        &format!(r#"{{"path":"{src}","elements":[{{"type":"paragraph","text":"highlight me"}}]}}"#),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    let r = call(
        office__pdf_highlight,
        &format!(r#"{{"path":"{src}","page":1,"rect":[72,700,300,720],"color":[255,0,0]}}"#),
    );
    assert_eq!(r["ok"], true, "highlight: {r}");

    // removing a different subtype leaves it; removing Highlight clears exactly one
    let keep = tmp("phl_keep.pdf");
    let rk = call(
        office__pdf_remove_annotations,
        &format!(r#"{{"path":"{src}","subtype":"Link","output":"{keep}"}}"#),
    );
    assert_eq!(rk["removed"], 0, "no links to remove: {rk}");
    let out = tmp("phl_out.pdf");
    let rr = call(
        office__pdf_remove_annotations,
        &format!(r#"{{"path":"{src}","subtype":"Highlight","output":"{out}"}}"#),
    );
    assert_eq!(rr["removed"], 1, "one highlight removed: {rr}");

    for f in [&src, &keep, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_annotations_lists_all() {
    let src = tmp("pann.pdf");
    let b = call(
        office__pdf_build,
        &format!(r#"{{"path":"{src}","elements":[{{"type":"paragraph","text":"doc"}}]}}"#),
    );
    assert_eq!(b["ok"], true, "build: {b}");
    call(
        office__pdf_add_link,
        &format!(r#"{{"path":"{src}","page":1,"url":"https://x.test","rect":[10,10,100,30]}}"#),
    );
    call(
        office__pdf_highlight,
        &format!(r#"{{"path":"{src}","page":1,"rect":[10,40,100,60]}}"#),
    );

    let a = call(office__pdf_annotations, &format!(r#"{{"path":"{src}"}}"#));
    assert_eq!(a["count"], 2, "two annotations: {a}");
    let subtypes: Vec<&str> = a["annotations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["subtype"].as_str().unwrap())
        .collect();
    assert!(subtypes.contains(&"Link"), "has Link: {a}");
    assert!(subtypes.contains(&"Highlight"), "has Highlight: {a}");
    // the Link entry exposes its uri
    let link = a["annotations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["subtype"] == "Link")
        .unwrap();
    assert_eq!(link["uri"], "https://x.test", "link uri: {a}");

    std::fs::remove_file(&src).ok();
}

#[test]
fn pdf_split_bookmarks_by_chapter() {
    // build a 4-page pdf
    let src = tmp("pbm.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[
                {{"type":"paragraph","text":"p1"}},
                {{"type":"pagebreak"}},
                {{"type":"paragraph","text":"p2"}},
                {{"type":"pagebreak"}},
                {{"type":"paragraph","text":"p3"}},
                {{"type":"pagebreak"}},
                {{"type":"paragraph","text":"p4"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");
    // bookmark Ch1 at page 1, Ch2 at page 3
    let so = call(
        office__pdf_set_outline,
        &format!(
            r#"{{"path":"{src}","outline":[{{"title":"Ch1","page":1}},{{"title":"Ch2","page":3}}]}}"#
        ),
    );
    assert_eq!(so["ok"], true, "set_outline: {so}");

    let dir = tmp("pbm_out");
    let r = call(
        office__pdf_split_bookmarks,
        &format!(r#"{{"path":"{src}","dir":"{dir}","prefix":"ch"}}"#),
    );
    assert_eq!(r["count"], 2, "two chapters: {r}");
    let files = r["files"].as_array().unwrap();
    // each chapter spans 2 pages (1-2 and 3-4)
    let i1 = call(office__pdf_info, &format!(r#"{{"path":{}}}"#, files[0]));
    assert_eq!(i1["pages"], 2, "chapter 1 has 2 pages: {i1}");
    let i2 = call(office__pdf_info, &format!(r#"{{"path":{}}}"#, files[1]));
    assert_eq!(i2["pages"], 2, "chapter 2 has 2 pages: {i2}");

    std::fs::remove_file(&src).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pdf_page_sizes_per_page() {
    let src = tmp("psizes.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[
                {{"type":"paragraph","text":"p1"}},
                {{"type":"pagebreak"}},
                {{"type":"paragraph","text":"p2"}}
            ]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    let r = call(office__pdf_page_sizes, &format!(r#"{{"path":"{src}"}}"#));
    assert_eq!(r["count"], 2, "two pages: {r}");
    let pages = r["pages"].as_array().unwrap();
    assert_eq!(pages[0]["page"], 1, "1-based: {r}");
    let w = pages[0]["width"].as_f64().unwrap();
    let h = pages[0]["height"].as_f64().unwrap();
    assert!(w > 0.0 && h > 0.0, "positive dims: {r}");
    assert!(h > w, "A4 portrait (height > width): {r}");

    std::fs::remove_file(&src).ok();
}

#[test]
fn pdf_assemble_mixed_inputs() {
    // one image and one 1-page pdf
    let img = tmp("asm.png");
    let h = call(
        office__img_new,
        r#"{"width":40,"height":40,"color":[10,20,30,255]}"#,
    );
    call(
        office__img_save,
        &format!(
            r#"{{"handle":{},"path":"{img}"}}"#,
            h["handle"].as_u64().unwrap()
        ),
    );
    let doc = tmp("asm.pdf");
    call(
        office__pdf_build,
        &format!(r#"{{"path":"{doc}","elements":[{{"type":"heading","level":1,"text":"Doc"}}]}}"#),
    );

    // image, pdf, image -> 3 pages
    let out = tmp("asm_out.pdf");
    let r = call(
        office__pdf_assemble,
        &format!(r#"{{"inputs":["{img}","{doc}","{img}"],"output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "assemble: {r}");
    assert_eq!(r["pages"], 3, "3 pages from 3 inputs: {r}");
    let info = call(office__pdf_info, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(info["pages"], 3, "info confirms 3 pages: {info}");

    for f in [&img, &doc, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_stamp_image_onto_pages() {
    // a 2-page PDF and a logo image
    let src = tmp("stamp.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[{{"type":"heading","level":1,"text":"A"}},{{"type":"pagebreak"}},{{"type":"heading","level":1,"text":"B"}}]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");
    let logo = tmp("logo.png");
    let n = call(
        office__img_new,
        r#"{"width":32,"height":32,"color":[255,0,0,255]}"#,
    );
    call(
        office__img_save,
        &format!(
            r#"{{"handle":{},"path":"{logo}"}}"#,
            n["handle"].as_u64().unwrap()
        ),
    );
    call(
        office__img_close,
        &format!(r#"{{"handle":{}}}"#, n["handle"].as_u64().unwrap()),
    );

    let out = tmp("stamp_out.pdf");
    let r = call(
        office__pdf_stamp_image,
        &format!(
            r#"{{"path":"{src}","image":"{logo}","output":"{out}","x":20,"y":20,"width":48,"height":48}}"#
        ),
    );
    assert_eq!(r["stamped"], 2, "stamped both pages: {r}");
    // output is still a valid 2-page PDF
    let info = call(office__pdf_info, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(info["pages"], 2, "pages preserved: {info}");

    for f in [&src, &logo, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_insert_at_position() {
    // base A,B,C and an insert X
    let base = tmp("ins_base.pdf");
    call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{base}","elements":[{{"type":"heading","level":1,"text":"Aye"}},{{"type":"pagebreak"}},{{"type":"heading","level":1,"text":"Bee"}},{{"type":"pagebreak"}},{{"type":"heading","level":1,"text":"Cee"}}]}}"#
        ),
    );
    let ins = tmp("ins_x.pdf");
    call(
        office__pdf_build,
        &format!(r#"{{"path":"{ins}","elements":[{{"type":"heading","level":1,"text":"Ex"}}]}}"#),
    );

    // insert after page 1 -> Aye, Ex, Bee, Cee
    let out = tmp("ins_out.pdf");
    let r = call(
        office__pdf_insert,
        &format!(r#"{{"path":"{base}","insert":"{ins}","position":1,"output":"{out}"}}"#),
    );
    assert_eq!(r["pages"], 4, "4 pages after insert: {r}");
    let rd = call(office__pdf_read, &format!(r#"{{"path":"{out}"}}"#));
    let txt = rd["text"].as_str().unwrap_or("");
    let (ia, ix, ib) = (txt.find("Aye"), txt.find("Ex"), txt.find("Bee"));
    assert!(ia < ix && ix < ib, "order Aye<Ex<Bee: {txt:?}");

    for f in [&base, &ins, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_draw_rect_on_pages() {
    let src = tmp("rect.pdf");
    call(
        office__pdf_build,
        &format!(r#"{{"path":"{src}","elements":[{{"type":"heading","level":1,"text":"H"}}]}}"#),
    );
    let out = tmp("rect_out.pdf");
    let r = call(
        office__pdf_draw_rect,
        &format!(
            r#"{{"path":"{src}","rects":[[50,50,100,80]],"color":[255,0,0],"output":"{out}"}}"#
        ),
    );
    assert_eq!(r["pages"], 1, "drawn on 1 page: {r}");
    assert_eq!(r["rects"], 1, "one rect: {r}");
    // output is still a valid PDF
    let info = call(office__pdf_info, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(info["pages"], 1, "valid pdf: {info}");

    for f in [&src, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_add_text_on_pages() {
    let src = tmp("addt.pdf");
    call(
        office__pdf_build,
        &format!(r#"{{"path":"{src}","elements":[{{"type":"heading","level":1,"text":"Body"}}]}}"#),
    );
    let out = tmp("addt_out.pdf");
    let r = call(
        office__pdf_add_text,
        &format!(r#"{{"path":"{src}","text":"CONFIDENTIAL","x":72,"y":700,"output":"{out}"}}"#),
    );
    assert_eq!(r["pages"], 1, "added on 1 page: {r}");
    let info = call(office__pdf_info, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(info["pages"], 1, "valid pdf: {info}");
    // the added text is extractable
    let rd = call(office__pdf_read, &format!(r#"{{"path":"{out}"}}"#));
    assert!(
        rd["text"].as_str().unwrap_or("").contains("CONFIDENTIAL"),
        "added text extractable: {}",
        rd["text"].as_str().unwrap_or("")
    );

    for f in [&src, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_draw_line_on_pages() {
    let src = tmp("line.pdf");
    call(
        office__pdf_build,
        &format!(r#"{{"path":"{src}","elements":[{{"type":"heading","level":1,"text":"H"}}]}}"#),
    );
    let out = tmp("line_out.pdf");
    let r = call(
        office__pdf_draw_line,
        &format!(r#"{{"path":"{src}","lines":[[50,50,300,50]],"width":2,"output":"{out}"}}"#),
    );
    assert_eq!(r["pages"], 1, "drawn on 1 page: {r}");
    assert_eq!(r["lines"], 1, "one line: {r}");
    let info = call(office__pdf_info, &format!(r#"{{"path":"{out}"}}"#));
    assert_eq!(info["pages"], 1, "valid pdf: {info}");

    for f in [&src, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn pdf_attach_list_and_extract() {
    let src = tmp("att_src.pdf");
    let b = call(
        office__pdf_build,
        &format!(
            r#"{{"path":"{src}","elements":[{{"type":"heading","level":1,"text":"Report"}}]}}"#
        ),
    );
    assert_eq!(b["ok"], true, "build: {b}");

    // two payload files
    let f1 = tmp("data1.csv");
    let f2 = tmp("data2.txt");
    std::fs::write(&f1, b"a,b,c\n1,2,3\n").unwrap();
    std::fs::write(&f2, b"hello attachment").unwrap();

    // attach first, then second (exercises the append path)
    let a1 = tmp("att1.pdf");
    let r1 = call(
        office__pdf_attach,
        &format!(r#"{{"path":"{src}","file":"{f1}","output":"{a1}","name":"data1.csv"}}"#),
    );
    assert_eq!(r1["count"], 1, "first attach -> 1: {r1}");
    let l1 = call(office__pdf_attachments, &format!(r#"{{"path":"{a1}"}}"#));
    assert_eq!(l1["count"], 1, "single-attach read-back: {l1}");
    let a2 = tmp("att2.pdf");
    let r2 = call(
        office__pdf_attach,
        &format!(r#"{{"path":"{a1}","file":"{f2}","output":"{a2}","name":"data2.txt"}}"#),
    );
    assert_eq!(r2["count"], 2, "second attach -> 2: {r2}");

    // host PDF still parses
    let info = call(office__pdf_info, &format!(r#"{{"path":"{a2}"}}"#));
    assert_eq!(info["pages"], 1, "host pages preserved: {info}");

    // list both
    let lst = call(office__pdf_attachments, &format!(r#"{{"path":"{a2}"}}"#));
    assert_eq!(lst["count"], 2, "two attachments: {lst}");
    let names: Vec<&str> = lst["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"data1.csv") && names.contains(&"data2.txt"),
        "names: {lst}"
    );

    // extract and verify bytes round-trip
    let dir = tmp("att_out");
    std::fs::create_dir_all(&dir).unwrap();
    let ex = call(
        office__pdf_attachments,
        &format!(r#"{{"path":"{a2}","extract_dir":"{dir}"}}"#),
    );
    assert_eq!(ex["count"], 2, "extract count: {ex}");
    assert_eq!(
        std::fs::read(format!("{dir}/data1.csv")).unwrap(),
        b"a,b,c\n1,2,3\n",
        "csv bytes round-trip"
    );
    assert_eq!(
        std::fs::read(format!("{dir}/data2.txt")).unwrap(),
        b"hello attachment",
        "txt bytes round-trip"
    );

    for f in [&src, &f1, &f2, &a1, &a2] {
        std::fs::remove_file(f).ok();
    }
    std::fs::remove_dir_all(&dir).ok();
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
fn img_resize_file_exact_and_fit() {
    let src = tmp("rf.png");
    let n = call(
        office__img_new,
        r#"{"width":100,"height":50,"color":[9,9,9,255]}"#,
    );
    call(
        office__img_save,
        &format!(
            r#"{{"handle":{},"path":"{src}"}}"#,
            n["handle"].as_u64().unwrap()
        ),
    );
    call(
        office__img_close,
        &format!(r#"{{"handle":{}}}"#, n["handle"].as_u64().unwrap()),
    );

    let ex = tmp("rf_ex.png");
    let r = call(
        office__img_resize_file,
        &format!(r#"{{"input":"{src}","output":"{ex}","width":40,"height":20}}"#),
    );
    assert_eq!(r["width"], 40, "exact w: {r}");
    assert_eq!(r["height"], 20, "exact h: {r}");

    let ft = tmp("rf_ft.png");
    let r2 = call(
        office__img_resize_file,
        &format!(r#"{{"input":"{src}","output":"{ft}","max":40}}"#),
    );
    assert_eq!(r2["width"], 40, "fit w: {r2}");
    assert_eq!(r2["height"], 20, "fit h (aspect): {r2}");

    for f in [&src, &ex, &ft] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn contact_sheet_grid() {
    // three small images of different colors
    let mut files = Vec::new();
    for (i, color) in [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]]
        .iter()
        .enumerate()
    {
        let p = tmp(&format!("cs{i}.png"));
        let n = call(
            office__img_new,
            &format!(r#"{{"width":20,"height":20,"color":{color:?}}}"#),
        );
        let h = n["handle"].as_u64().unwrap();
        call(
            office__img_save,
            &format!(r#"{{"handle":{h},"path":"{p}"}}"#),
        );
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
        files.push(p);
    }

    let out = tmp("cs_out.png");
    let list = files
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(",");
    let r = call(
        office__contact_sheet,
        &format!(r#"{{"images":[{list}],"output":"{out}","cols":2,"thumb":16}}"#),
    );
    assert_eq!(r["count"], 3, "three images: {r}");
    assert!(r["width"].as_u64().unwrap_or(0) > 0, "grid has width: {r}");
    // output opens as a valid image
    let i = call(office__img_open, &format!(r#"{{"path":"{out}"}}"#));
    let h = i["handle"].as_u64().expect("opens");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    for f in files.iter().chain([&out]) {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn img_data_uri_encodes() {
    let png = tmp("uri.png");
    let n = call(
        office__img_new,
        r#"{"width":8,"height":8,"color":[1,2,3,255]}"#,
    );
    call(
        office__img_save,
        &format!(
            r#"{{"handle":{},"path":"{png}"}}"#,
            n["handle"].as_u64().unwrap()
        ),
    );
    let r = call(office__img_data_uri, &format!(r#"{{"path":"{png}"}}"#));
    assert_eq!(r["mime"], "image/png", "mime: {r}");
    let uri = r["data_uri"].as_str().unwrap();
    assert!(
        uri.starts_with("data:image/png;base64,"),
        "prefix: {uri:.40}"
    );
    // the base64 payload decodes back to a valid image
    let b64 = uri.strip_prefix("data:image/png;base64,").unwrap();
    let back = call(office__img_from_base64, &format!(r#"{{"base64":"{b64}"}}"#));
    assert_eq!(back["width"], 8, "round-trips to 8px image: {back}");

    std::fs::remove_file(&png).ok();
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
fn image_phash_dedup() {
    // a horizontal gradient (textured) vs a flat image -> different hashes
    let g = call(
        office__img_new,
        r#"{"width":64,"height":64,"color":[0,0,0,255]}"#,
    );
    let gh = g["handle"].as_u64().unwrap();
    call(
        office__img_gradient,
        &format!(
            r#"{{"handle":{gh},"kind":"linear","angle":0,"from":[0,0,0,255],"to":[255,255,255,255]}}"#
        ),
    );
    let flat = call(
        office__img_new,
        r#"{"width":64,"height":64,"color":[128,128,128,255]}"#,
    );
    let fh = flat["handle"].as_u64().unwrap();

    // self-distance is 0, similarity 1
    let self_h = call(
        office__img_phash,
        &format!(r#"{{"handle":{gh},"other":{gh}}}"#),
    );
    assert_eq!(
        self_h["distance"].as_u64().unwrap(),
        0,
        "self distance 0: {self_h}"
    );
    assert_eq!(
        self_h["similarity"].as_f64().unwrap(),
        1.0,
        "self similarity 1: {self_h}"
    );
    // hash is a 16-char hex string
    assert_eq!(
        self_h["hash"].as_str().unwrap().len(),
        16,
        "16-char hex: {self_h}"
    );

    // gradient vs flat differ
    let cross = call(
        office__img_phash,
        &format!(r#"{{"handle":{gh},"other":{fh}}}"#),
    );
    assert!(
        cross["distance"].as_u64().unwrap() > 0,
        "different images differ: {cross}"
    );

    call(office__img_close, &format!(r#"{{"handle":{gh}}}"#));
    call(office__img_close, &format!(r#"{{"handle":{fh}}}"#));
}

#[test]
fn image_crop_aspect_centered() {
    // 100x100 -> crop to 16:9 keeps full width, height = 100/(16/9) ≈ 56
    let n = call(
        office__img_new,
        r#"{"width":100,"height":100,"color":[10,20,30,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    let r = call(
        office__img_crop_aspect,
        &format!(r#"{{"handle":{h},"aspect":[16,9]}}"#),
    );
    assert_eq!(
        r["width"].as_u64().unwrap(),
        100,
        "wide aspect keeps width: {r}"
    );
    assert_eq!(
        r["height"].as_u64().unwrap(),
        56,
        "height = 100*9/16 rounded: {r}"
    );
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    // ratio form: square 1:1 from a 80x40 image -> 40x40
    let n2 = call(
        office__img_new,
        r#"{"width":80,"height":40,"color":[1,2,3,255]}"#,
    );
    let h2 = n2["handle"].as_u64().unwrap();
    let r2 = call(
        office__img_crop_aspect,
        &format!(r#"{{"handle":{h2},"ratio":1.0}}"#),
    );
    assert_eq!(r2["width"].as_u64().unwrap(), 40, "square width: {r2}");
    assert_eq!(r2["height"].as_u64().unwrap(), 40, "square height: {r2}");
    call(office__img_close, &format!(r#"{{"handle":{h2}}}"#));
}

#[test]
fn image_average_color_mean() {
    // solid color -> average equals that color
    let n = call(
        office__img_new,
        r#"{"width":20,"height":20,"color":[40,80,120,255]}"#,
    );
    let h = n["handle"].as_u64().unwrap();
    let a = call(office__img_average_color, &format!(r#"{{"handle":{h}}}"#));
    assert_eq!(a["r"].as_u64().unwrap(), 40, "mean R: {a}");
    assert_eq!(a["g"].as_u64().unwrap(), 80, "mean G: {a}");
    assert_eq!(a["b"].as_u64().unwrap(), 120, "mean B: {a}");
    assert_eq!(a["hex"], "#285078", "hex of mean: {a}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
}

#[test]
fn image_brightness_dark_light() {
    // near-black -> dark
    let dark = call(
        office__img_new,
        r#"{"width":10,"height":10,"color":[10,10,10,255]}"#,
    );
    let dh = dark["handle"].as_u64().unwrap();
    let d = call(office__img_brightness, &format!(r#"{{"handle":{dh}}}"#));
    assert!(
        (d["brightness"].as_f64().unwrap() - 10.0).abs() < 1e-6,
        "dark luma ~10: {d}"
    );
    assert_eq!(d["is_dark"], true, "near-black is dark: {d}");
    call(office__img_close, &format!(r#"{{"handle":{dh}}}"#));

    // white -> light
    let light = call(
        office__img_new,
        r#"{"width":10,"height":10,"color":[255,255,255,255]}"#,
    );
    let lh = light["handle"].as_u64().unwrap();
    let l = call(office__img_brightness, &format!(r#"{{"handle":{lh}}}"#));
    assert!(
        (l["brightness"].as_f64().unwrap() - 255.0).abs() < 1e-6,
        "white luma 255: {l}"
    );
    assert_eq!(l["is_dark"], false, "white is not dark: {l}");
    call(office__img_close, &format!(r#"{{"handle":{lh}}}"#));
}

#[test]
fn color_contrast_wcag() {
    // black on white -> maximal 21:1, passes everything
    let bw = call(office__color_contrast, r#"{"a":[0,0,0],"b":[255,255,255]}"#);
    assert_eq!(
        bw["ratio"].as_f64().unwrap(),
        21.0,
        "black/white = 21: {bw}"
    );
    assert_eq!(bw["aa"], true, "AA pass: {bw}");
    assert_eq!(bw["aaa"], true, "AAA pass: {bw}");

    // light gray on white -> low contrast, fails AA
    let lg = call(
        office__color_contrast,
        r#"{"a":[200,200,200],"b":[255,255,255]}"#,
    );
    assert!(lg["ratio"].as_f64().unwrap() < 2.0, "low contrast: {lg}");
    assert_eq!(lg["aa"], false, "fails AA: {lg}");
}

#[test]
fn color_info_breakdown() {
    // pure red -> #ff0000, HSL (0, 1, 0.5)
    let r = call(office__color_info, r#"{"color":[255,0,0]}"#);
    assert_eq!(r["hex"], "#ff0000", "red hex: {r}");
    assert_eq!(r["rgb"][0].as_u64().unwrap(), 255, "rgb r: {r}");
    let hsl = r["hsl"].as_array().unwrap();
    assert_eq!(hsl[0].as_f64().unwrap(), 0.0, "hue 0: {r}");
    assert_eq!(hsl[1].as_f64().unwrap(), 1.0, "saturation 1: {r}");
    assert_eq!(hsl[2].as_f64().unwrap(), 0.5, "lightness 0.5: {r}");

    // pure green -> hue 120
    let g = call(office__color_info, r#"{"color":[0,255,0]}"#);
    assert_eq!(g["hsl"][0].as_f64().unwrap(), 120.0, "green hue 120: {g}");
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
fn kde_curve_integrates_to_one() {
    // symmetric data centered at 5; KDE over a wide grid should integrate ~1
    let data = [3.0, 4.0, 4.0, 5.0, 5.0, 5.0, 6.0, 6.0, 7.0];
    let pts = kde_curve(&data, 0.0, 10.0, 256);
    assert_eq!(pts.len(), 256, "grid length");
    // trapezoidal integral of the density over [0,10] ≈ 1
    let dx = 10.0 / 255.0;
    let area: f64 = pts.windows(2).map(|w| (w[0].1 + w[1].1) / 2.0 * dx).sum();
    assert!((area - 1.0).abs() < 0.05, "area ~1, got {area}");
    // densest grid point sits near the data center (5)
    let peak =
        pts.iter().cloned().fold(
            (0.0, f64::NEG_INFINITY),
            |acc, p| {
                if p.1 > acc.1 {
                    p
                } else {
                    acc
                }
            },
        );
    assert!((peak.0 - 5.0).abs() < 1.0, "peak near 5, got {}", peak.0);
}

#[test]
fn chart_density_kde_raster_and_svg() {
    // two series of raw values -> overlaid KDE curves
    let series =
        r#"[{"name":"a","data":[1,2,2,3,3,3,4,4,5]},{"name":"b","data":[4,5,5,6,6,6,7,7,8]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"density","width":480,"height":320,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("density raster: {c}"));
    assert_eq!(c["type"], "density", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    // SVG: filled area path + outline polyline, well-formed document
    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"density","series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "density svg malformed"
    );
    assert!(
        svg.contains("fill-opacity=\"0.35\""),
        "density svg filled area"
    );
    assert!(svg.contains("<polyline"), "density svg outline");
    // both series names appear in the legend
    assert!(
        svg.contains(">a<") && svg.contains(">b<"),
        "density svg legend entries"
    );
}

#[test]
fn chart_violin_raster_and_svg() {
    // two groups of raw values -> side-by-side violins
    let series = r#"[{"name":"ctrl","data":[2,3,3,4,4,4,5,5,6]},{"name":"treat","data":[5,6,6,7,7,7,8,8,9]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"violin","width":480,"height":340,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("violin raster: {c}"));
    assert_eq!(c["type"], "violin", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"violin","series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "violin svg malformed"
    );
    // mirrored KDE shape is a closed path; white median tick present
    assert!(
        svg.contains("<path") && svg.contains("Z\""),
        "violin svg closed shape"
    );
    assert!(svg.contains("stroke=\"#ffffff\""), "violin svg median tick");
    // category labels under the axis
    assert!(
        svg.contains(">ctrl<") && svg.contains(">treat<"),
        "violin svg category labels"
    );
}

#[test]
fn ecdf_steps_monotone_zero_to_one() {
    let data = [3.0, 1.0, 2.0, 4.0]; // n=4
    let v = ecdf_steps(&data, 0.0, 5.0);
    // starts at (xlo, 0), ends at (xhi, 1)
    assert_eq!(v.first().copied(), Some((0.0, 0.0)), "starts at 0");
    assert_eq!(v.last().copied(), Some((5.0, 1.0)), "ends at 1");
    // y is non-decreasing across the whole step sequence
    let mut prev = -1.0;
    for &(_, y) in &v {
        assert!(y >= prev - 1e-12, "monotone non-decreasing");
        prev = y;
    }
    // after the value 2 (the 2nd smallest) the level is exactly 2/4 = 0.5
    assert!(
        v.iter()
            .any(|&(x, y)| (x - 2.0).abs() < 1e-9 && (y - 0.5).abs() < 1e-9),
        "ecdf at 2 = 0.5: {v:?}"
    );
}

#[test]
fn chart_ecdf_raster_and_svg() {
    let series = r#"[{"name":"a","data":[1,2,3,4,5,6,7,8]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"ecdf","width":460,"height":320,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("ecdf raster: {c}"));
    assert_eq!(c["type"], "ecdf", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"ecdf","series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "ecdf svg malformed"
    );
    assert!(svg.contains("<polyline"), "ecdf svg step curve");
    // y axis runs 0.0..1.0 (cumulative probability ticks)
    assert!(
        svg.contains(">0.0<") && svg.contains(">1.0<"),
        "ecdf svg probability axis"
    );
}

#[test]
fn norm_ppf_known_quantiles() {
    assert!(norm_ppf(0.5).abs() < 1e-6, "median quantile is 0");
    assert!(
        (norm_ppf(0.975) - 1.959_963_98).abs() < 1e-4,
        "97.5% ~ 1.96"
    );
    assert!(
        (norm_ppf(0.025) + 1.959_963_98).abs() < 1e-4,
        "2.5% ~ -1.96"
    );
    // strictly increasing
    assert!(norm_ppf(0.1) < norm_ppf(0.9), "monotone");
}

#[test]
fn chart_qq_raster_and_svg() {
    let series = r#"[{"name":"x","data":[1,2,3,4,5,6,7,8,9,10]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"qq","width":460,"height":360,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("qq raster: {c}"));
    assert_eq!(c["type"], "qq", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"qq","series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "qq svg malformed"
    );
    // point cloud (circles) + a qq reference line
    assert!(svg.contains("<circle"), "qq svg points");
    assert!(svg.contains("<line"), "qq svg reference line");
}

#[test]
fn chart_ribbon_raster_and_svg() {
    // a confidence band: [lo,hi] per x
    let series = r#"[{"name":"ci","data":[[1,3],[2,5],[2,6],[1,4],[0,3]]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"ribbon","width":460,"height":320,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("ribbon raster: {c}"));
    assert_eq!(c["type"], "ribbon", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"ribbon","series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "ribbon svg malformed"
    );
    // one closed translucent band polygon
    assert!(
        svg.contains("<path") && svg.contains("fill-opacity=\"0.35\"") && svg.contains("Z\""),
        "ribbon svg band polygon"
    );
}

#[test]
fn chart_jitter_strip_raster_and_svg() {
    let series = r#"[{"name":"grp","data":[3,3,3,4,4,5,5,5,6]}]"#;
    // jitter: points spread; reproducible for a fixed seed
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"jitter","width":420,"height":320,"seed":7,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("jitter raster: {c}"));
    assert_eq!(c["type"], "jitter", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v1 = call(
        office__chart_svg,
        &format!(r#"{{"type":"jitter","seed":7,"series":{series}}}"#),
    );
    let v2 = call(
        office__chart_svg,
        &format!(r#"{{"type":"jitter","seed":7,"series":{series}}}"#),
    );
    let svg = v1["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "jitter svg malformed"
    );
    assert!(svg.contains("<circle"), "jitter svg points");
    // nine data points -> nine circles
    assert_eq!(svg.matches("<circle").count(), 9, "one circle per value");
    // same seed -> identical output (reproducible jitter)
    assert_eq!(
        svg,
        v2["svg"].as_str().unwrap_or(""),
        "seeded jitter reproducible"
    );

    // strip: no jitter -> all points share the slot center x
    let strip = call(
        office__chart_svg,
        &format!(r#"{{"type":"strip","series":{series}}}"#),
    );
    let ssvg = strip["svg"].as_str().unwrap_or("");
    let xs: std::collections::HashSet<&str> = ssvg
        .match_indices("<circle cx=\"")
        .map(|(i, _)| {
            let rest = &ssvg[i + 12..];
            &rest[..rest.find('"').unwrap_or(0)]
        })
        .collect();
    assert_eq!(xs.len(), 1, "strip points aligned on one x: {xs:?}");
}

#[test]
fn chart_rug_raster_and_svg() {
    let series = r#"[{"name":"x","data":[1,2,4,4,7,9]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"rug","width":420,"height":260,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("rug raster: {c}"));
    assert_eq!(c["type"], "rug", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"rug","series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "rug svg malformed"
    );
    // ticks are stroke-width 1.5 lines (axis/grid lines have no such attr) -> 6 values
    assert_eq!(
        svg.matches("stroke-width=\"1.5\"").count(),
        6,
        "one rug tick per value"
    );
}

#[test]
fn beeswarm_offsets_avoid_overlap() {
    // ten identical y values must fan out so no two points overlap in x
    let ys = vec![100.0; 10];
    let radius = 3.0;
    let offs = beeswarm_offsets(&ys, radius, 200.0);
    assert_eq!(offs.len(), 10);
    let mut sorted = offs.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    for w in sorted.windows(2) {
        assert!(
            (w[1] - w[0]).abs() >= 2.0 * radius - 1e-6,
            "adjacent same-y points separated by >= diameter: {sorted:?}"
        );
    }
    // points at well-separated y can share x = 0 (no horizontal spread needed)
    let ys2 = vec![0.0, 100.0, 200.0];
    let offs2 = beeswarm_offsets(&ys2, radius, 200.0);
    assert!(
        offs2.iter().all(|&o| o.abs() < 1e-9),
        "no spread when y separated: {offs2:?}"
    );
}

#[test]
fn chart_beeswarm_raster_and_svg() {
    let series = r#"[{"name":"g","data":[4,4,4,4,4,5,5,3,3,6]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"beeswarm","width":420,"height":340,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("beeswarm raster: {c}"));
    assert_eq!(c["type"], "beeswarm", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"beeswarm","series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "beeswarm svg malformed"
    );
    // one circle per value (10)
    assert_eq!(svg.matches("<circle").count(), 10, "one point per value");
}

#[test]
fn marching_squares_encloses_peak() {
    // 3x3 grid, single peak at the center -> level 0.5 yields a closed 4-seg loop
    #[rustfmt::skip]
    let grid = vec![
        0.0, 0.0, 0.0,
        0.0, 1.0, 0.0,
        0.0, 0.0, 0.0,
    ];
    let mut segs = 0;
    marching_squares(&grid, 3, 3, 0.5, |_, _, _, _| segs += 1);
    assert_eq!(segs, 4, "contour around a single peak is a 4-segment loop");
}

#[test]
fn chart_contour_raster_and_svg() {
    // two clusters of points -> 2-D density contours
    let series = r#"[{"name":"pts","data":[[1,1],[1.2,0.9],[0.9,1.1],[1,1.2],[5,5],[5.1,4.9],[4.8,5.2],[5,5.1],[2,2],[3,3]]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"contour","width":460,"height":380,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("contour raster: {c}"));
    assert_eq!(c["type"], "contour", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"contour","series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "contour svg malformed"
    );
    // contour line paths present plus faint underlying points
    assert!(svg.contains("<path"), "contour svg iso-lines");
    assert!(svg.contains("<circle"), "contour svg underlying points");
}

#[test]
fn chart_ridgeline_raster_and_svg() {
    // three groups -> three stacked density ridges
    let series = r#"[{"name":"a","data":[1,2,2,3,3,3,4,4,5]},{"name":"b","data":[3,4,4,5,5,5,6,6,7]},{"name":"c","data":[5,6,6,7,7,7,8,8,9]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"ridgeline","width":480,"height":360,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("ridgeline raster: {c}"));
    assert_eq!(c["type"], "ridgeline", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"ridgeline","series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "ridgeline svg malformed"
    );
    // one filled ridge path per series (3)
    assert_eq!(
        svg.matches("fill-opacity=\"0.6\"").count(),
        3,
        "one ridge per group"
    );
    // group labels present
    assert!(
        svg.contains(">a<") && svg.contains(">b<") && svg.contains(">c<"),
        "ridgeline group labels"
    );
}

#[test]
fn loess_fit_recovers_linear_trend() {
    // perfectly linear y = 2x + 1 -> LOESS should reproduce it closely
    let pts: Vec<(f64, f64)> = (0..20).map(|i| (i as f64, 2.0 * i as f64 + 1.0)).collect();
    let grid = [2.0, 9.0, 17.0];
    let fit = loess_fit(&pts, 0.5, &grid);
    assert_eq!(fit.len(), 3);
    for &(x, yhat) in &fit {
        let expected = 2.0 * x + 1.0;
        assert!(
            (yhat - expected).abs() < 0.25,
            "loess ~ linear at x={x}: got {yhat}, want {expected}"
        );
    }
}

#[test]
fn chart_smooth_loess_raster_and_svg() {
    // noisy-ish upward data
    let series =
        r#"[{"name":"s","data":[[1,2],[2,2.5],[3,4],[4,3.5],[5,6],[6,5.5],[7,8],[8,7.5],[9,10]]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"smooth","width":460,"height":340,"span":0.6,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("smooth raster: {c}"));
    assert_eq!(c["type"], "smooth", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"loess","series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "smooth svg malformed"
    );
    // the LOESS curve is a thick polyline over faint points
    assert!(
        svg.contains("stroke-width=\"2.5\""),
        "smooth svg loess curve"
    );
    assert!(svg.contains("<circle"), "smooth svg underlying points");
}

#[test]
fn chart_bin2d_raster_and_svg() {
    // points clustered into a few cells
    let series = r#"[{"name":"d","data":[[1,1],[1,1],[1,1],[2,2],[2,2],[5,5],[1,5],[5,1]]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"bin2d","width":420,"height":340,"bins":5,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("bin2d raster: {c}"));
    assert_eq!(c["type"], "bin2d", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"bin2d","bins":5,"series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "bin2d svg malformed"
    );
    // colored count cells are <rect> fills (only nonzero cells drawn)
    assert!(svg.contains("<rect x="), "bin2d svg cells");
}

#[test]
fn chart_pairs_splom_raster_and_svg() {
    // three variables (columns) of equal length -> 3x3 scatterplot matrix
    let series = r#"[{"name":"mpg","data":[21,22,20,25,30]},{"name":"hp","data":[110,120,90,80,70]},{"name":"wt","data":[2.6,2.9,3.1,2.2,2.0]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"pairs","width":480,"height":480,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("pairs raster: {c}"));
    assert_eq!(c["type"], "pairs", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"splom","series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "pairs svg malformed"
    );
    // 3x3 = 9 panel borders
    assert_eq!(
        svg.matches("fill=\"none\" stroke=\"#d2d2d2\"").count(),
        9,
        "9 panels in a 3x3 matrix"
    );
    // diagonal carries the variable names (once each, anchored)
    assert!(
        svg.contains(">mpg<") && svg.contains(">hp<") && svg.contains(">wt<"),
        "diagonal variable labels"
    );
    // legend suppressed for SPLOM -> names appear once (diagonal only)
    assert_eq!(svg.matches(">mpg<").count(), 1, "no duplicate legend label");
}

#[test]
fn hclust_merges_tight_clusters_first() {
    // two tight pairs near 0 and near 100; the cross merge must be the last (root)
    let feats = vec![vec![0.0], vec![0.5], vec![100.0], vec![100.5]];
    let (nodes, root) = hclust(&feats);
    // n=4 leaves -> 3 internal merges, ids 4,5,6; root is the highest
    assert_eq!(nodes.len(), 7, "4 leaves + 3 merges");
    assert_eq!(root, 6, "last merge is the root");
    // root height (cross-cluster ~100) far exceeds the earlier within-pair merges
    let root_h = nodes[root].2;
    let inner_max = nodes[4..6].iter().map(|&(_, _, h)| h).fold(0.0, f64::max);
    assert!(
        root_h > inner_max * 10.0,
        "root merge dwarfs the tight merges: {root_h} vs {inner_max}"
    );
}

#[test]
fn chart_dendrogram_raster_and_svg() {
    let series = r#"[{"name":"a","data":[0,0]},{"name":"b","data":[0.4,0.2]},{"name":"c","data":[9,9]},{"name":"d","data":[9.3,8.8]}]"#;
    let c = call(
        office__chart_render,
        &format!(r#"{{"type":"dendrogram","width":460,"height":360,"series":{series}}}"#),
    );
    let h = c["handle"]
        .as_u64()
        .unwrap_or_else(|| panic!("dendrogram raster: {c}"));
    assert_eq!(c["type"], "dendrogram", "type echoed: {c}");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    let v = call(
        office__chart_svg,
        &format!(r#"{{"type":"hclust","series":{series}}}"#),
    );
    let svg = v["svg"].as_str().unwrap_or("");
    assert!(
        svg.starts_with("<svg") && svg.ends_with("</svg>"),
        "dendrogram svg malformed"
    );
    // 3 merge brackets (n-1 polylines) + 4 leaf labels
    assert_eq!(svg.matches("<polyline").count(), 3, "n-1 merge brackets");
    assert!(
        svg.contains(">a<") && svg.contains(">b<") && svg.contains(">c<") && svg.contains(">d<"),
        "leaf labels"
    );
}

#[test]
fn chart_types_render_raster_and_svg() {
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
fn chart_types_and_overlays() {
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
fn chart_types_theming_smooth() {
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
fn chart_calendar_parallel_hexbin() {
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
                "properties":{{"title":"Q Report","author":"jacob","company":"MenkeTech","subject":"sales"}},
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
fn barcode_save_to_files() {
    // QR straight to a PNG
    let qr = tmp("bc_qr.png");
    let r = call(
        office__barcode_save,
        &format!(r#"{{"data":"hello world","output":"{qr}"}}"#),
    );
    assert_eq!(r["ok"], true, "qr save: {r}");
    assert_eq!(r["kind"], "qr", "kind: {r}");
    let i = call(office__img_open, &format!(r#"{{"path":"{qr}"}}"#));
    let h = i["handle"].as_u64().expect("qr opens as image");
    call(office__img_close, &format!(r#"{{"handle":{h}}}"#));

    // 1D barcode straight to a PNG
    let bc = tmp("bc_1d.png");
    let r2 = call(
        office__barcode_save,
        &format!(r#"{{"data":"STRYKE-2026","output":"{bc}","kind":"1d","symbology":"code128"}}"#),
    );
    assert_eq!(r2["ok"], true, "1d save: {r2}");
    assert!(
        std::fs::metadata(&bc).map(|m| m.len() > 0).unwrap_or(false),
        "1d file written"
    );

    std::fs::remove_file(&qr).ok();
    std::fs::remove_file(&bc).ok();
}

#[test]
fn barcode_sheet_batch() {
    let path = tmp("bcsheet.xlsx");
    let w = call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{path}","sheets":[{{"name":"D","rows":[["sku"],["A100"],["B200"],[""]]}}]}}"#
        ),
    );
    assert_eq!(w["ok"], true, "write: {w}");

    let dir = tmp("bcsheet_out");
    std::fs::create_dir_all(&dir).unwrap();
    let r = call(
        office__barcode_sheet,
        &format!(r#"{{"path":"{path}","column":"sku","dir":"{dir}","prefix":"qr-"}}"#),
    );
    assert_eq!(r["count"], 2, "blank skipped, two codes: {r}");
    // each generated file opens as an image
    for f in r["files"].as_array().unwrap() {
        let i = call(office__img_open, &format!(r#"{{"path":{f}}}"#));
        let h = i["handle"].as_u64().expect("barcode opens as image");
        call(office__img_close, &format!(r#"{{"handle":{h}}}"#));
    }
    assert!(
        std::path::Path::new(&format!("{dir}/qr-A100.png")).exists(),
        "named file exists"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_dir_all(&dir).ok();
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
            r#"{{"path":"{path}","props":{{"title":"Q2 Report","author":"Jacob","subject":"sales","keywords":"q2,sales","company":"MenkeTechnologies"}}}}"#
        ),
    );
    assert_eq!(m["ok"], true, "meta_write: {m}");
    let r = call(office__meta_read, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["title"], "Q2 Report", "{r}");
    assert_eq!(r["author"], "Jacob");
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

    let docx = tmp("extractimg.docx");
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
            r#"{{"path":"{path}","output":"{out}","values":{{"name":"Jacob Menke","agree":true}}}}"#
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
    assert_eq!(name_v, "Jacob Menke", "text field filled: {r2}");
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
        &[("{{name}}".to_string(), "Jacob".to_string())],
    );
    assert_eq!(n, 1, "one substitution");
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("Hello Jacob!"), "joined into first run: {s}");
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
            r#"{{"path":"{path}","replace":{{"{{{{name}}}}":"Jacob","{{{{id}}}}":"INV-42"}}}}"#
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
        joined.contains("Dear Jacob, your invoice INV-42 is ready."),
        "{joined}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn mail_merge_per_record() {
    // template with {{name}} / {{city}} placeholders
    let tmpl = tmp("mm_tmpl.docx");
    call(
        office__doc_write,
        &format!(
            r#"{{"path":"{tmpl}","blocks":[{{"kind":"para","text":"Hello {{{{name}}}} from {{{{city}}}}"}}]}}"#
        ),
    );
    // data sheet
    let data = tmp("mm_data.xlsx");
    call(
        office__sheet_write,
        &format!(
            r#"{{"path":"{data}","sheets":[{{"name":"D","rows":[["name","city"],["Alice","NYC"],["Bob","LA"]]}}]}}"#
        ),
    );

    let dir = tmp("mm_out");
    std::fs::create_dir_all(&dir).unwrap();
    let r = call(
        office__mail_merge,
        &format!(r#"{{"template":"{tmpl}","data":"{data}","dir":"{dir}","name_field":"name"}}"#),
    );
    assert_eq!(r["count"], 2, "two merged docs: {r}");
    let a = call(
        office__doc_read,
        &format!(r#"{{"path":"{dir}/Alice.docx"}}"#),
    );
    let ja = a["paragraphs"].to_string();
    assert!(ja.contains("Hello Alice from NYC"), "Alice filled: {ja}");
    assert!(!ja.contains("{{"), "no leftover placeholders: {ja}");
    let b = call(office__doc_read, &format!(r#"{{"path":"{dir}/Bob.docx"}}"#));
    assert!(
        b["paragraphs"].to_string().contains("Hello Bob from LA"),
        "Bob filled: {b}"
    );

    std::fs::remove_file(&tmpl).ok();
    std::fs::remove_file(&data).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn text_replace_plain_file() {
    let path = tmp("tr.md");
    std::fs::write(&path, "Hello world hello").unwrap();

    let out = tmp("tr_out.md");
    let r = call(
        office__text_replace,
        &format!(
            r#"{{"path":"{path}","replace":{{"hello":"hi"}},"ignore_case":true,"output":"{out}"}}"#
        ),
    );
    assert_eq!(r["replaced"], 2, "two replacements: {r}");
    let text = std::fs::read_to_string(&out).unwrap();
    assert_eq!(text, "hi world hi", "ci replaced both: {text:?}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn text_sed_regex_replace() {
    let path = tmp("sed.txt");
    std::fs::write(&path, "2026-06-14 log\n2025-01-02 old\n").unwrap();

    // reformat YYYY-MM-DD -> MM/DD/YYYY via backreferences, global
    let out = tmp("sed_out.txt");
    let r = call(
        office__text_sed,
        &serde_json::json!({
            "path": path, "output": out,
            "pattern": r"(\d{4})-(\d{2})-(\d{2})", "replacement": "$2/$3/$1"
        })
        .to_string(),
    );
    assert_eq!(r["replaced"], 2, "two dates rewritten: {r}");
    let t = std::fs::read_to_string(&out).unwrap();
    assert!(
        t.contains("06/14/2026 log"),
        "first date reformatted: {t:?}"
    );
    assert!(
        t.contains("01/02/2025 old"),
        "second date reformatted: {t:?}"
    );

    // non-global: only the first match
    let out1 = tmp("sed_one.txt");
    let r1 = call(
        office__text_sed,
        &serde_json::json!({
            "path": path, "output": out1,
            "pattern": r"\d{4}", "replacement": "YEAR", "global": false
        })
        .to_string(),
    );
    assert_eq!(r1["replaced"], 1, "non-global replaces one: {r1}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&out1).ok();
}

#[test]
fn text_extract_matches() {
    let path = tmp("extract.txt");
    std::fs::write(&path, "contact bob@acme.com or bob@acme.com or sue@x.org\n").unwrap();

    // all email matches (with a repeat)
    let r = call(
        office__text_extract,
        &serde_json::json!({ "path": path, "pattern": r"[\w.]+@[\w.]+" }).to_string(),
    );
    assert_eq!(r["count"], 3, "three matches incl repeat: {r}");
    assert_eq!(r["matches"][0], "bob@acme.com", "first match: {r}");

    // unique de-dupes
    let ru = call(
        office__text_extract,
        &serde_json::json!({ "path": path, "pattern": r"[\w.]+@[\w.]+", "unique": true })
            .to_string(),
    );
    assert_eq!(ru["count"], 2, "two distinct: {ru}");

    // capture group 1: just the domain
    let rg = call(
        office__text_extract,
        &serde_json::json!({ "path": path, "pattern": r"@([\w.]+)", "group": 1, "unique": true })
            .to_string(),
    );
    assert_eq!(rg["matches"][0], "acme.com", "domain group: {rg}");
    assert_eq!(rg["matches"][1], "x.org", "second domain: {rg}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn text_grep_matches_lines() {
    let path = tmp("grep.txt");
    std::fs::write(&path, "alpha\nbeta FOO\ngamma\nfoo bar\n").unwrap();

    // literal, case-sensitive: only "foo bar"
    let r = call(
        office__text_grep,
        &format!(r#"{{"path":"{path}","query":"foo"}}"#),
    );
    assert_eq!(r["count"], 1, "one case-sensitive match: {r}");
    assert_eq!(r["matches"][0]["line"], 4, "1-based line: {r}");
    assert_eq!(r["matches"][0]["text"], "foo bar", "matched text: {r}");

    // ignore_case: "beta FOO" and "foo bar"
    let ci = call(
        office__text_grep,
        &format!(r#"{{"path":"{path}","query":"foo","ignore_case":true}}"#),
    );
    assert_eq!(ci["count"], 2, "two case-insensitive matches: {ci}");

    // invert: lines NOT containing "a" -> none? "foo bar" has no 'a'? it has 'a' in bar. all have a except...
    let inv = call(
        office__text_grep,
        &format!(r#"{{"path":"{path}","query":"foo","invert":true}}"#),
    );
    assert_eq!(inv["count"], 3, "three non-matching lines: {inv}");

    // regex mode: lines starting with f or g
    let rx = call(
        office__text_grep,
        &format!(r#"{{"path":"{path}","query":"^[fg]","regex":true}}"#),
    );
    assert_eq!(
        rx["count"], 2,
        "regex ^[fg] matches gamma and foo bar: {rx}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn text_stats_wc() {
    let path = tmp("wc.txt");
    std::fs::write(&path, "a b\nc\n").unwrap();

    let r = call(office__text_stats, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(r["lines"], 2, "two lines: {r}");
    assert_eq!(r["words"], 3, "three words: {r}");
    assert_eq!(r["chars"], 6, "six chars incl newlines: {r}");
    assert_eq!(r["bytes"], 6, "six bytes: {r}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn text_sort_lines() {
    let path = tmp("sort.txt");
    std::fs::write(&path, "b\na\nc\na\n").unwrap();

    // ascending + unique -> a, b, c
    let out = tmp("sort_out.txt");
    let r = call(
        office__text_sort,
        &format!(r#"{{"path":"{path}","output":"{out}","unique":true}}"#),
    );
    assert_eq!(r["lines"], 3, "deduped to 3 lines: {r}");
    let t = std::fs::read_to_string(&out).unwrap();
    assert_eq!(
        t.lines().collect::<Vec<_>>(),
        vec!["a", "b", "c"],
        "sorted unique: {t:?}"
    );

    // descending (no unique) -> c, b, a, a
    let outd = tmp("sort_d.txt");
    call(
        office__text_sort,
        &format!(r#"{{"path":"{path}","output":"{outd}","descending":true}}"#),
    );
    let td = std::fs::read_to_string(&outd).unwrap();
    assert_eq!(
        td.lines().next().unwrap(),
        "c",
        "descending first is c: {td:?}"
    );

    for f in [&path, &out, &outd] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn text_uniq_adjacent_and_global() {
    let path = tmp("uniq.txt");
    // adjacent dup of "a"; "b" repeats non-adjacently.
    std::fs::write(&path, "a\na\nb\nc\nb\n").unwrap();

    // default: collapse adjacent only → a, b, c, b
    let out = tmp("uniq_adj.txt");
    let r = call(
        office__text_uniq,
        &format!(r#"{{"path":"{path}","output":"{out}"}}"#),
    );
    assert_eq!(r["lines"], 4, "adjacent collapse keeps 4: {r}");
    let t = std::fs::read_to_string(&out).unwrap();
    assert_eq!(
        t.lines().collect::<Vec<_>>(),
        vec!["a", "b", "c", "b"],
        "adjacent uniq: {t:?}"
    );

    // count mode → "2\ta" first line
    let outc = tmp("uniq_c.txt");
    call(
        office__text_uniq,
        &format!(r#"{{"path":"{path}","output":"{outc}","count":true}}"#),
    );
    let tc = std::fs::read_to_string(&outc).unwrap();
    assert_eq!(tc.lines().next().unwrap(), "2\ta", "count prefix: {tc:?}");

    // global: dedupe non-adjacent too → a, b, c
    let outg = tmp("uniq_g.txt");
    let rg = call(
        office__text_uniq,
        &format!(r#"{{"path":"{path}","output":"{outg}","global":true}}"#),
    );
    assert_eq!(rg["lines"], 3, "global dedupe keeps 3: {rg}");
    let tg = std::fs::read_to_string(&outg).unwrap();
    assert_eq!(
        tg.lines().collect::<Vec<_>>(),
        vec!["a", "b", "c"],
        "global uniq preserves first-seen order: {tg:?}"
    );

    for f in [&path, &out, &outc, &outg] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn text_cut_fields() {
    let path = tmp("cut.csv");
    std::fs::write(&path, "a,b,c\n1,2,3\n4,5,6\n").unwrap();

    // pick fields 3 and 1 (reordered), comma delimiter
    let r = call(
        office__text_cut,
        &format!(r#"{{"path":"{path}","delim":",","fields":[3,1]}}"#),
    );
    assert_eq!(r["count"], 3, "three lines: {r}");
    assert_eq!(r["lines"][0], "c,a", "header reordered: {r}");
    assert_eq!(r["lines"][1], "3,1", "row1 reordered: {r}");

    // out-of-range field -> empty; custom output_delim
    let r2 = call(
        office__text_cut,
        &format!(r#"{{"path":"{path}","delim":",","fields":[1,9],"output_delim":"|"}}"#),
    );
    assert_eq!(r2["lines"][1], "1|", "missing field is empty: {r2}");

    std::fs::remove_file(&path).ok();
}

#[test]
fn text_wrap_lines() {
    let path = tmp("wrap.txt");
    // one long line + a blank line preserved
    std::fs::write(&path, "the quick brown fox jumps\n\nshort\n").unwrap();

    let out = tmp("wrap_out.txt");
    let r = call(
        office__text_wrap,
        &format!(r#"{{"path":"{path}","output":"{out}","width":10}}"#),
    );
    assert_eq!(r["ok"], true, "wrap: {r}");
    let t = std::fs::read_to_string(&out).unwrap();
    let lines: Vec<&str> = t.lines().collect();
    // greedy pack to width 10: "the quick" (9), "brown fox" (9), "jumps"
    assert_eq!(lines[0], "the quick", "first wrapped line: {t:?}");
    assert_eq!(lines[1], "brown fox", "second wrapped line: {t:?}");
    assert_eq!(lines[2], "jumps", "remainder: {t:?}");
    assert_eq!(lines[3], "", "blank line preserved: {t:?}");
    assert_eq!(lines[4], "short", "short line kept: {t:?}");
    // each non-blank wrapped line is within width
    assert!(
        lines.iter().all(|l| l.chars().count() <= 10),
        "all <= width: {t:?}"
    );

    // break_words: a single over-long token is hard-split
    let path2 = tmp("wrap2.txt");
    std::fs::write(&path2, "supercalifragilistic\n").unwrap();
    let out2 = tmp("wrap2_out.txt");
    call(
        office__text_wrap,
        &format!(r#"{{"path":"{path2}","output":"{out2}","width":8,"break_words":true}}"#),
    );
    let t2 = std::fs::read_to_string(&out2).unwrap();
    assert_eq!(
        t2.lines().next().unwrap(),
        "supercal",
        "hard-broken word: {t2:?}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
    std::fs::remove_file(&path2).ok();
    std::fs::remove_file(&out2).ok();
}

#[test]
fn text_tr_translate_delete_squeeze() {
    let path = tmp("tr.txt");
    std::fs::write(&path, "Hello World").unwrap();

    // translate lowercase -> uppercase via ranges
    let out = tmp("tr_up.txt");
    let r = call(
        office__text_tr,
        &format!(r#"{{"path":"{path}","from":"a-z","to":"A-Z","output":"{out}"}}"#),
    );
    assert_eq!(r["ok"], true, "tr: {r}");
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "HELLO WORLD",
        "ranged translate"
    );

    // shorter set2: vowels -> '*' (last char repeats)
    let outv = tmp("tr_vowel.txt");
    call(
        office__text_tr,
        &format!(r#"{{"path":"{path}","from":"aeiou","to":"*","output":"{outv}"}}"#),
    );
    assert_eq!(
        std::fs::read_to_string(&outv).unwrap(),
        "H*ll* W*rld",
        "short set2 repeats last"
    );

    // delete digits
    let pathd = tmp("tr_del.txt");
    std::fs::write(&pathd, "a1b2c3").unwrap();
    let outd = tmp("tr_del_out.txt");
    call(
        office__text_tr,
        &format!(r#"{{"path":"{pathd}","from":"0-9","delete":true,"output":"{outd}"}}"#),
    );
    assert_eq!(
        std::fs::read_to_string(&outd).unwrap(),
        "abc",
        "delete digit range"
    );

    // squeeze repeated spaces
    let paths = tmp("tr_sq.txt");
    std::fs::write(&paths, "a    b   c").unwrap();
    let outs = tmp("tr_sq_out.txt");
    call(
        office__text_tr,
        &format!(r#"{{"path":"{paths}","from":" ","squeeze":true,"output":"{outs}"}}"#),
    );
    assert_eq!(
        std::fs::read_to_string(&outs).unwrap(),
        "a b c",
        "squeeze spaces"
    );

    for f in [&path, &out, &outv, &pathd, &outd, &paths, &outs] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn text_paste_side_by_side() {
    let a = tmp("paste_a.txt");
    let b = tmp("paste_b.txt");
    std::fs::write(&a, "1\n2\n3\n").unwrap();
    std::fs::write(&b, "x\ny\n").unwrap(); // shorter -> padded
    let out = tmp("paste_out.txt");
    let r = call(
        office__text_paste,
        &format!(r#"{{"paths":["{a}","{b}"],"delim":",","output":"{out}"}}"#),
    );
    assert_eq!(r["count"].as_u64().unwrap(), 3, "three merged lines: {r}");
    let t = std::fs::read_to_string(&out).unwrap();
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines[0], "1,x", "line 1 paired");
    assert_eq!(lines[1], "2,y", "line 2 paired");
    assert_eq!(lines[2], "3,", "shorter file padded with empty field");

    for f in [&a, &b, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn text_comm_set_compare() {
    let a = tmp("comm_a.txt");
    let b = tmp("comm_b.txt");
    std::fs::write(&a, "apple\nbanana\ncherry\n").unwrap();
    std::fs::write(&b, "banana\ncherry\ndate\n").unwrap();
    let r = call(office__text_comm, &format!(r#"{{"a":"{a}","b":"{b}"}}"#));
    assert_eq!(
        r["only_a"].as_array().unwrap().len(),
        1,
        "apple only in a: {r}"
    );
    assert_eq!(r["only_a"][0], "apple", "only_a content: {r}");
    assert_eq!(r["only_b"][0], "date", "only_b content: {r}");
    assert_eq!(
        r["common"].as_u64().unwrap(),
        2,
        "banana+cherry common: {r}"
    );
    let both: Vec<&str> = r["both"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        both.contains(&"banana") && both.contains(&"cherry"),
        "both: {r}"
    );

    // case-insensitive folds Apple==apple
    std::fs::write(&b, "APPLE\nkiwi\n").unwrap();
    let ri = call(
        office__text_comm,
        &format!(r#"{{"a":"{a}","b":"{b}","ignore_case":true}}"#),
    );
    assert_eq!(
        ri["common"].as_u64().unwrap(),
        1,
        "apple matches APPLE: {ri}"
    );

    for f in [&a, &b] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn text_join_on_key() {
    let a = tmp("join_a.txt");
    let b = tmp("join_b.txt");
    std::fs::write(&a, "1,alice\n2,bob\n3,carol\n").unwrap();
    std::fs::write(&b, "1,90\n3,75\n4,50\n").unwrap();
    let out = tmp("join_out.txt");
    let r = call(
        office__text_join,
        &format!(r#"{{"a":"{a}","b":"{b}","field":1,"delim":",","output":"{out}"}}"#),
    );
    // ids 1 and 3 match -> 2 joined lines
    assert_eq!(r["count"].as_u64().unwrap(), 2, "two matched keys: {r}");
    let t = std::fs::read_to_string(&out).unwrap();
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines[0], "1,alice,90", "joined row 1: {t:?}");
    assert_eq!(lines[1], "3,carol,75", "joined row 3: {t:?}");

    for f in [&a, &b, &out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn text_shuf_reproducible() {
    let path = tmp("shuf.txt");
    std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
    let out = tmp("shuf_out.txt");
    let r = call(
        office__text_shuf,
        &format!(r#"{{"path":"{path}","output":"{out}","seed":7}}"#),
    );
    assert_eq!(r["lines"].as_u64().unwrap(), 5, "all lines kept: {r}");
    let t1 = std::fs::read_to_string(&out).unwrap();
    // same set of lines (permutation, none lost/added)
    let mut got: Vec<&str> = t1.lines().collect();
    got.sort_unstable();
    assert_eq!(
        got,
        vec!["a", "b", "c", "d", "e"],
        "permutation preserves set"
    );

    // same seed -> identical output (reproducible)
    let out2 = tmp("shuf_out2.txt");
    call(
        office__text_shuf,
        &format!(r#"{{"path":"{path}","output":"{out2}","seed":7}}"#),
    );
    assert_eq!(
        t1,
        std::fs::read_to_string(&out2).unwrap(),
        "seeded shuffle reproducible"
    );

    for f in [&path, &out, &out2] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn text_base64_roundtrip() {
    let path = tmp("b64.bin");
    let content = "Hello, World! \u{2014} café \u{1F600}";
    std::fs::write(&path, content).unwrap();

    // encode -> returns base64 + writes it
    let enc_out = tmp("b64.txt");
    let e = call(
        office__text_base64,
        &format!(r#"{{"path":"{path}","output":"{enc_out}"}}"#),
    );
    assert_eq!(e["ok"], true, "encode: {e}");
    let b64 = e["base64"].as_str().unwrap();
    assert!(!b64.is_empty(), "base64 produced: {e}");
    assert_eq!(
        std::fs::read_to_string(&enc_out).unwrap(),
        b64,
        "written matches returned"
    );

    // decode -> original bytes recovered
    let dec_out = tmp("b64_decoded.bin");
    let d = call(
        office__text_base64,
        &format!(r#"{{"path":"{enc_out}","decode":true,"output":"{dec_out}"}}"#),
    );
    assert_eq!(d["ok"], true, "decode: {d}");
    assert_eq!(
        std::fs::read_to_string(&dec_out).unwrap(),
        content,
        "roundtrip preserves content"
    );

    for f in [&path, &enc_out, &dec_out] {
        std::fs::remove_file(f).ok();
    }
}

#[test]
fn text_hash_checksums() {
    let path = tmp("hash.txt");
    // canonical CRC-32 check value for "123456789" is 0xCBF43926
    std::fs::write(&path, "123456789").unwrap();
    let h = call(office__text_hash, &format!(r#"{{"path":"{path}"}}"#));
    assert_eq!(h["bytes"].as_u64().unwrap(), 9, "byte count: {h}");
    assert_eq!(h["crc32"], "cbf43926", "canonical CRC-32 check value: {h}");
    // FNV-1a 64 is 16 hex chars
    assert_eq!(
        h["fnv1a64"].as_str().unwrap().len(),
        16,
        "fnv hex width: {h}"
    );

    // different content -> different crc
    std::fs::write(&path, "123456780").unwrap();
    let h2 = call(office__text_hash, &format!(r#"{{"path":"{path}"}}"#));
    assert_ne!(h2["crc32"], h["crc32"], "crc changes with content");

    std::fs::remove_file(&path).ok();
}

#[test]
fn text_redact_pii() {
    let path = tmp("redact.txt");
    std::fs::write(
        &path,
        "Contact jane@example.com or 555-123-4567. SSN 123-45-6789.",
    )
    .unwrap();
    let out = tmp("redact_out.txt");
    let r = call(
        office__text_redact,
        &format!(r#"{{"path":"{path}","output":"{out}","patterns":["email","ssn"],"mask":"XXX"}}"#),
    );
    assert_eq!(
        r["redactions"].as_u64().unwrap(),
        2,
        "email + ssn masked: {r}"
    );
    let t = std::fs::read_to_string(&out).unwrap();
    assert!(!t.contains("jane@example.com"), "email gone: {t}");
    assert!(!t.contains("123-45-6789"), "ssn gone: {t}");
    assert!(t.contains("XXX"), "mask applied: {t}");
    // phone was not in the selected patterns -> still present
    assert!(t.contains("555-123-4567"), "phone untouched: {t}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn text_template_fill() {
    let path = tmp("template.txt");
    std::fs::write(
        &path,
        "Dear {{name}}, your balance is {{amount}}. {{missing}}",
    )
    .unwrap();
    let out = tmp("template_out.txt");
    let r = call(
        office__text_template,
        &format!(
            r#"{{"path":"{path}","output":"{out}","data":{{"name":"Jane","amount":42}},"missing":"blank"}}"#
        ),
    );
    assert_eq!(
        r["replaced"].as_u64().unwrap(),
        2,
        "two placeholders filled: {r}"
    );
    let t = std::fs::read_to_string(&out).unwrap();
    assert_eq!(
        t, "Dear Jane, your balance is 42. ",
        "filled + blanked: {t:?}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn text_tac_reverse_lines() {
    let path = tmp("tac.txt");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
    let out = tmp("tac_out.txt");
    let r = call(
        office__text_tac,
        &format!(r#"{{"path":"{path}","output":"{out}"}}"#),
    );
    assert_eq!(r["lines"].as_u64().unwrap(), 3, "three lines: {r}");
    let t = std::fs::read_to_string(&out).unwrap();
    assert_eq!(t, "three\ntwo\none\n", "line order reversed: {t:?}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn text_head_and_tail() {
    let path = tmp("head.txt");
    std::fs::write(&path, "l1\nl2\nl3\nl4\nl5\n").unwrap();

    let h = call(office__text_head, &format!(r#"{{"path":"{path}","n":2}}"#));
    assert_eq!(h["count"], 2, "head 2: {h}");
    assert_eq!(h["lines"][0], "l1", "first line: {h}");
    assert_eq!(h["lines"][1], "l2", "second line: {h}");

    let t = call(
        office__text_head,
        &format!(r#"{{"path":"{path}","n":2,"tail":true}}"#),
    );
    assert_eq!(t["lines"][0], "l4", "tail first: {t}");
    assert_eq!(t["lines"][1], "l5", "tail last: {t}");

    // write the head slice to a file
    let out = tmp("head_out.txt");
    call(
        office__text_head,
        &format!(r#"{{"path":"{path}","n":1,"output":"{out}"}}"#),
    );
    let written = std::fs::read_to_string(&out).unwrap();
    assert_eq!(written.trim(), "l1", "head slice written: {written:?}");

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&out).ok();
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
