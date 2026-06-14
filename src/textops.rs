// Document text search/replace — template / mail-merge filling for office
// documents. The hard part is OOXML: a single placeholder like `{{name}}` is
// routinely split across several `<w:t>` runs (spell-check, tracked formatting),
// so a naive per-text-node replace misses it. The fix is per-paragraph run
// coalescing: join all the run text in a paragraph, replace on the joined
// string, then write the whole result back into the first run and blank the
// rest. Paragraphs with no match are passed through untouched.
//
// This is format-internal plumbing (it rewrites the run-text XML parts inside
// the zip, reusing `rewrite_zip`), not generic string work.

/// The (paragraph element, run-text element) to coalesce for a given part.
/// `text_tag` of `None` means "capture every text node in the paragraph"
/// (ODF, where text sits directly in `text:p` as well as in `text:span`).
fn replace_spec(ext: &str, name: &str) -> Option<(&'static str, Option<&'static str>)> {
    let xml = |n: &str| n.ends_with(".xml");
    match ext {
        "docx" | "docm" => {
            let hdr_ftr = (name.starts_with("word/header") || name.starts_with("word/footer"))
                && xml(name);
            (name == "word/document.xml" || hdr_ftr).then_some(("w:p", Some("w:t")))
        }
        "pptx" | "pptm" => ((name.starts_with("ppt/slides/slide")
            || name.starts_with("ppt/notesSlides/notesSlide"))
            && xml(name))
        .then_some(("a:p", Some("a:t"))),
        "xlsx" | "xlsm" => {
            if name == "xl/sharedStrings.xml" {
                Some(("si", Some("t")))
            } else if name.starts_with("xl/worksheets/sheet") && xml(name) {
                Some(("is", Some("t"))) // inline strings
            } else {
                None
            }
        }
        "ods" | "odt" | "odp" | "odg" => {
            (name == "content.xml" || name == "styles.xml").then_some(("text:p", None))
        }
        _ => None,
    }
}

/// Apply the replacement list to one paragraph's joined run text. If anything
/// changed, write the new string into the first captured text node and clear
/// the others. Returns the number of substitutions made.
fn coalesce_paragraph(
    events: &mut [quick_xml::events::Event<'static>],
    text_idx: &[usize],
    repls: &[(String, String)],
) -> usize {
    use quick_xml::events::{BytesText, Event};
    if text_idx.is_empty() {
        return 0;
    }
    let mut joined = String::new();
    for &i in text_idx {
        if let Event::Text(t) = &events[i] {
            if let Ok(s) = t.xml10_content() {
                joined.push_str(&s);
            }
        }
    }
    let mut new = joined.clone();
    let mut n = 0usize;
    for (find, rep) in repls {
        if find.is_empty() {
            continue;
        }
        let hits = new.matches(find.as_str()).count();
        if hits > 0 {
            n += hits;
            new = new.replace(find.as_str(), rep);
        }
    }
    if n == 0 {
        return 0;
    }
    events[text_idx[0]] = Event::Text(BytesText::new(&new).into_owned());
    for &i in &text_idx[1..] {
        events[i] = Event::Text(BytesText::new("").into_owned());
    }
    n
}

/// Stream `xml`, coalescing+replacing each `para_tag` paragraph, and return the
/// rewritten bytes plus the substitution count. Content outside paragraphs is
/// copied through verbatim.
fn replace_in_xml(
    xml: &[u8],
    para_tag: &str,
    text_tag: Option<&str>,
    repls: &[(String, String)],
) -> (Vec<u8>, usize) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut writer = quick_xml::Writer::new(Vec::new());
    let mut buf = Vec::new();
    let mut count = 0usize;

    let mut depth = 0i32; // paragraph nesting (0 = outside)
    let mut text_depth = 0i32; // depth inside a text_tag element
    let mut para: Vec<Event<'static>> = Vec::new();
    let mut text_idx: Vec<usize> = Vec::new();
    let para_b = para_tag.as_bytes();
    let text_b = text_tag.map(str::as_bytes);

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let is_para = e.name().as_ref() == para_b;
                if depth > 0 {
                    if let Some(tb) = text_b {
                        if e.name().as_ref() == tb {
                            text_depth += 1;
                        }
                    }
                    if is_para {
                        depth += 1;
                    }
                    para.push(Event::Start(e.into_owned()));
                } else if is_para {
                    depth = 1;
                    text_depth = 0;
                    para.clear();
                    text_idx.clear();
                    para.push(Event::Start(e.into_owned()));
                } else {
                    let _ = writer.write_event(Event::Start(e));
                }
            }
            Ok(Event::Text(e)) => {
                if depth > 0 {
                    let capture = match text_b {
                        Some(_) => text_depth > 0,
                        None => true,
                    };
                    if capture {
                        text_idx.push(para.len());
                    }
                    para.push(Event::Text(e.into_owned()));
                } else {
                    let _ = writer.write_event(Event::Text(e));
                }
            }
            Ok(Event::End(e)) => {
                let is_para = e.name().as_ref() == para_b;
                if depth > 0 {
                    if let Some(tb) = text_b {
                        if e.name().as_ref() == tb && text_depth > 0 {
                            text_depth -= 1;
                        }
                    }
                    para.push(Event::End(e.into_owned()));
                    if is_para {
                        depth -= 1;
                        if depth == 0 {
                            count += coalesce_paragraph(&mut para, &text_idx, repls);
                            for ev in para.drain(..) {
                                let _ = writer.write_event(ev);
                            }
                        }
                    }
                } else {
                    let _ = writer.write_event(Event::End(e));
                }
            }
            Ok(Event::Eof) => break,
            Ok(other) => {
                if depth > 0 {
                    para.push(other.into_owned());
                } else {
                    let _ = writer.write_event(other);
                }
            }
            Err(_) => break,
        }
        buf.clear();
    }
    (writer.into_inner(), count)
}

fn parse_replacements(opts: &Value) -> Vec<(String, String)> {
    let mut v = Vec::new();
    if let Some(obj) = opts.get("replace").and_then(Value::as_object) {
        for (k, val) in obj {
            v.push((k.clone(), cell_to_string(val)));
        }
    }
    if let Some(arr) = opts.get("replacements").and_then(Value::as_array) {
        for r in arr {
            if let Some(f) = r.get("find").and_then(Value::as_str) {
                let rep = r.get("replace").map(cell_to_string).unwrap_or_default();
                v.push((f.to_string(), rep));
            }
        }
    }
    v
}

/// Search/replace text across a document's run-text parts. opts: `path`,
/// `replace` ({find: replacement}) and/or `replacements` ([{find, replace}]),
/// `output` (defaults to `path`, edits in place). Works on OOXML
/// (docx/pptx/xlsx incl. headers/footers, slides, shared+inline strings) and
/// ODF (content + styles). Returns `{ok, path, replaced}`.
fn op_replace_text(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let repls = parse_replacements(&opts);
    if repls.is_empty() {
        return Err(anyhow!("no replacements supplied (use `replace` or `replacements`)"));
    }
    let ext = ext_of(path);
    if !(is_ooxml(&ext) || is_odf(&ext)) {
        return Err(anyhow!("unsupported format for text replace: {ext}"));
    }
    let bytes = std::fs::read(path)?;
    let mut replace_map: std::collections::HashMap<String, Vec<u8>> =
        std::collections::HashMap::new();
    let mut total = 0usize;
    for name in zip_entry_names(&bytes)? {
        let Some((para, text_tag)) = replace_spec(&ext, &name) else {
            continue;
        };
        let Ok(xml) = read_zip_entry(&bytes, &name) else {
            continue;
        };
        let (new_xml, n) = replace_in_xml(&xml, para, text_tag, &repls);
        if n > 0 {
            total += n;
            replace_map.insert(name, new_xml);
        }
    }
    let new_bytes = rewrite_zip(&bytes, &replace_map, false)?;
    std::fs::write(&output, new_bytes)?;
    Ok(json!({"ok": true, "path": output, "replaced": total}))
}

/// Mail-merge: fill a `{{field}}`-placeholder template once per data record,
/// writing one document per record. opts: template (docx/pptx/xlsx/odf),
/// data => spreadsheet path (read as records) or records => [objects],
/// dir => output directory, sheet => source sheet selector, prefix => filename
/// prefix, name_field => field whose value names each file (default: 1-based
/// index). Returns `{ count, files }`.
fn op_mail_merge(opts: Value) -> Result<Value> {
    let template = req_str(&opts, "template")?;
    let dir = req_str(&opts, "dir")?;
    let ext = ext_of(template);
    let prefix = opts.get("prefix").and_then(Value::as_str).unwrap_or("");
    let name_field = opts.get("name_field").and_then(Value::as_str);

    let records: Vec<Value> = if let Some(r) = opts.get("records").and_then(Value::as_array) {
        r.clone()
    } else if let Some(data) = opts.get("data").and_then(Value::as_str) {
        op_sheet_records(json!({ "path": data, "sheet": opts.get("sheet") }))?
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    } else {
        return Err(anyhow!("need data (spreadsheet path) or records (array)"));
    };

    let mut files = Vec::new();
    for (i, rec) in records.iter().enumerate() {
        let mut map = serde_json::Map::new();
        if let Some(o) = rec.as_object() {
            for (k, v) in o {
                map.insert(format!("{{{{{k}}}}}"), Value::String(cell_to_string(v)));
            }
        }
        let label = match name_field.and_then(|f| rec.get(f)) {
            Some(v) => {
                let safe: String = cell_to_string(v)
                    .chars()
                    .map(|c| {
                        if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                let safe = safe.trim().to_string();
                if safe.is_empty() {
                    (i + 1).to_string()
                } else {
                    safe
                }
            }
            None => (i + 1).to_string(),
        };
        let out = format!("{dir}/{prefix}{label}.{ext}");
        op_replace_text(json!({ "path": template, "replace": Value::Object(map), "output": out }))?;
        files.push(out);
    }
    Ok(json!({ "count": files.len(), "files": files }))
}

/// Find/replace in a plain-text file (md/html/txt/csv/json/rtf/…) — the text
/// counterpart to `replace_text` (binary office) and `sheet_replace` (cells).
/// opts: path, output (default in place), `replace` => {find: repl} map or
/// `replacements` => [{find, replace}] list, ignore_case (ASCII). Returns
/// `{ ok, path, replaced }`.
fn op_text_replace(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let repls = parse_replacements(&opts);
    if repls.is_empty() {
        return Err(anyhow!("no replacements supplied (use `replace` or `replacements`)"));
    }
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);

    let mut text = std::fs::read_to_string(path)?;
    let mut total = 0usize;
    for (find, rep) in &repls {
        if find.is_empty() {
            continue;
        }
        if ignore_case {
            let (new, n) = ascii_ci_replace(&text, find, rep);
            total += n;
            text = new;
        } else {
            total += text.matches(find.as_str()).count();
            text = text.replace(find.as_str(), rep);
        }
    }
    std::fs::write(&output, &text)?;
    Ok(json!({ "ok": true, "path": output, "replaced": total }))
}

/// Regex find/replace over a text file (the `sed s/…/…/g` analogue) — unlike
/// `text_replace` (literal substring) this matches a regular expression and the
/// replacement may use capture-group backreferences (`$1`, `${name}`). opts:
/// path, output (default in place), pattern => regex (required), replacement =>
/// substitution string (default ""), global => replace every match (default
/// true; false replaces only the first), ignore_case (default false). Returns
/// `{ ok, path, replaced }` (number of matches substituted).
fn op_text_sed(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let pattern = req_str(&opts, "pattern")?;
    let replacement = opts
        .get("replacement")
        .and_then(Value::as_str)
        .unwrap_or("");
    let global = opts.get("global").and_then(flag_of).unwrap_or(true);
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);

    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| anyhow!("invalid regex: {e}"))?;
    let text = std::fs::read_to_string(path)?;
    let total = re.find_iter(&text).count();
    let new = if global {
        re.replace_all(&text, replacement).into_owned()
    } else {
        re.replace(&text, replacement).into_owned()
    };
    let replaced = if global { total } else { total.min(1) };
    std::fs::write(&output, new)?;
    Ok(json!({ "ok": true, "path": output, "replaced": replaced }))
}

/// Pull every regex match (or capture group) out of a text file into a list —
/// e.g. all emails, URLs, or numbers (the extract-all complement of `text_grep`,
/// which returns whole lines, and `text_sed`, which rewrites). opts: path,
/// pattern => regex (required), group => capture-group index to collect (default
/// 0 = whole match), unique => de-duplicate, preserving first-seen order (default
/// false), ignore_case (default false), output => write the matches one per line
/// to a file (omit to just return them). Returns `{ count, matches: [...], path? }`.
fn op_text_extract(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let pattern = req_str(&opts, "pattern")?;
    let group = opts.get("group").and_then(Value::as_u64).unwrap_or(0) as usize;
    let unique = opts.get("unique").and_then(flag_of).unwrap_or(false);
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);

    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| anyhow!("invalid regex: {e}"))?;
    let text = std::fs::read_to_string(path)?;
    let mut matches: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for caps in re.captures_iter(&text) {
        if let Some(m) = caps.get(group) {
            let s = m.as_str().to_string();
            if unique && !seen.insert(s.clone()) {
                continue;
            }
            matches.push(s);
        }
    }

    let mut out = json!({ "count": matches.len(), "matches": matches });
    if let Some(output) = opts.get("output").and_then(Value::as_str) {
        let joined = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(output, format!("{joined}\n"))?;
        out["path"] = json!(output);
    }
    Ok(out)
}

/// Grep matching lines from a text file (the line-oriented complement of
/// `doc_find`). opts: path, query (required, literal substring), ignore_case
/// (default false), invert => return lines that do NOT match (default false),
/// max => cap the number of matches. Returns `{ count, matches: [{ line, text }] }`
/// with 1-based line numbers.
fn op_text_grep(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let query = req_str(&opts, "query")?;
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);
    let invert = opts.get("invert").and_then(flag_of).unwrap_or(false);
    let max = opts.get("max").and_then(Value::as_u64).map(|n| n as usize);

    let text = String::from_utf8_lossy(&std::fs::read(path)?).into_owned();
    let needle = if ignore_case {
        query.to_lowercase()
    } else {
        query.to_string()
    };
    let mut matches = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let hay = if ignore_case {
            line.to_lowercase()
        } else {
            line.to_string()
        };
        let hit = hay.contains(&needle);
        if hit != invert {
            matches.push(json!({ "line": i + 1, "text": line }));
            if max.is_some_and(|m| matches.len() >= m) {
                break;
            }
        }
    }
    let count = matches.len();
    Ok(json!({ "count": count, "matches": matches }))
}

/// `wc`-style statistics for a raw text file. opts: path. Returns
/// `{ lines, words, chars, bytes }` — lines counted by `\n`-delimited content,
/// words by whitespace, chars by Unicode scalar, bytes by file size.
fn op_text_stats(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let raw = std::fs::read(path)?;
    let bytes = raw.len();
    let text = String::from_utf8_lossy(&raw);
    let lines = if text.is_empty() { 0 } else { text.lines().count() };
    let words = text.split_whitespace().count();
    let chars = text.chars().count();
    Ok(json!({ "lines": lines, "words": words, "chars": chars, "bytes": bytes }))
}

/// Sort the lines of a text file (`sort`/`uniq` in one). opts: path, output
/// (default in place), descending (default false), numeric => compare as numbers
/// (unparseable lines sort last), unique => drop duplicate lines, ignore_case =>
/// fold case when comparing/deduping (default false). Returns
/// `{ ok, path, lines }` (line count after sorting).
fn op_text_sort(opts: Value) -> Result<Value> {
    use std::cmp::Ordering;
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let descending = opts.get("descending").and_then(flag_of).unwrap_or(false);
    let numeric = opts.get("numeric").and_then(flag_of).unwrap_or(false);
    let unique = opts.get("unique").and_then(flag_of).unwrap_or(false);
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);

    let text = String::from_utf8_lossy(&std::fs::read(path)?).into_owned();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let key = |s: &str| -> String {
        if ignore_case {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    lines.sort_by(|a, b| {
        let ord = if numeric {
            let na = a.trim().parse::<f64>().unwrap_or(f64::INFINITY);
            let nb = b.trim().parse::<f64>().unwrap_or(f64::INFINITY);
            na.partial_cmp(&nb).unwrap_or(Ordering::Equal)
        } else {
            key(a).cmp(&key(b))
        };
        if descending {
            ord.reverse()
        } else {
            ord
        }
    });
    if unique {
        lines.dedup_by(|a, b| key(a) == key(b));
    }

    let out = format!("{}\n", lines.join("\n"));
    std::fs::write(&output, out)?;
    Ok(json!({ "ok": true, "path": output, "lines": lines.len() }))
}

/// Collapse duplicate lines (the `uniq(1)` analogue). By default only *adjacent*
/// duplicates are merged, preserving order; `global => true` removes every later
/// duplicate (keeping first occurrence) regardless of position. opts: path,
/// output (default in place), count => prefix each kept line with its occurrence
/// count and a tab (like `uniq -c`), ignore_case, global. Returns
/// `{ ok, path, lines }` (kept line count).
fn op_text_uniq(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let count = opts.get("count").and_then(flag_of).unwrap_or(false);
    let ignore_case = opts.get("ignore_case").and_then(flag_of).unwrap_or(false);
    let global = opts.get("global").and_then(flag_of).unwrap_or(false);

    let text = String::from_utf8_lossy(&std::fs::read(path)?).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    let key = |s: &str| -> String {
        if ignore_case {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };

    // Each kept entry is (display line, occurrence count).
    let mut kept: Vec<(String, u64)> = Vec::new();
    if global {
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for &line in &lines {
            let k = key(line);
            match seen.get(&k) {
                Some(&idx) => kept[idx].1 += 1,
                None => {
                    seen.insert(k, kept.len());
                    kept.push((line.to_string(), 1));
                }
            }
        }
    } else {
        for &line in &lines {
            match kept.last_mut() {
                Some(last) if key(&last.0) == key(line) => last.1 += 1,
                _ => kept.push((line.to_string(), 1)),
            }
        }
    }

    let rendered: Vec<String> = kept
        .iter()
        .map(|(line, c)| {
            if count {
                format!("{c}\t{line}")
            } else {
                line.clone()
            }
        })
        .collect();
    let out = format!("{}\n", rendered.join("\n"));
    std::fs::write(&output, out)?;
    Ok(json!({ "ok": true, "path": output, "lines": kept.len() }))
}

/// First (or last) N lines of a text file (`head`/`tail`). opts: path, n =>
/// number of lines (default 10), tail => take from the end (default false),
/// output => also write the slice to a file (omit to just return it). Returns
/// `{ count, lines: [...], path? }`.
fn op_text_head(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let n = opts.get("n").and_then(Value::as_u64).unwrap_or(10) as usize;
    let tail = opts.get("tail").and_then(flag_of).unwrap_or(false);

    let text = String::from_utf8_lossy(&std::fs::read(path)?).into_owned();
    let all: Vec<&str> = text.lines().collect();
    let slice: Vec<String> = if tail {
        all.iter().rev().take(n).rev().map(|s| s.to_string()).collect()
    } else {
        all.iter().take(n).map(|s| s.to_string()).collect()
    };

    let mut out = json!({ "count": slice.len(), "lines": slice });
    if let Some(output) = opts.get("output").and_then(Value::as_str) {
        std::fs::write(output, format!("{}\n", out["lines"]
            .as_array().unwrap().iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")))?;
        out["path"] = json!(output);
    }
    Ok(out)
}

/// Extract delimited fields from each line (the `cut -d -f` analogue). opts:
/// path, fields => array of 1-based field numbers in output order (required),
/// delim => input delimiter (default tab), output_delim => joiner for the picked
/// fields (default: same as `delim`), output => also write the result to a file
/// (omit to just return it). Out-of-range field numbers contribute an empty
/// string (so column counts stay stable). Returns `{ count, lines: [...], path? }`.
fn op_text_cut(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let delim = opts.get("delim").and_then(Value::as_str).unwrap_or("\t");
    if delim.is_empty() {
        return Err(anyhow!("delim must be non-empty"));
    }
    let out_delim = opts
        .get("output_delim")
        .and_then(Value::as_str)
        .unwrap_or(delim);
    let fields: Vec<usize> = opts
        .get("fields")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing fields (array of 1-based field numbers)"))?
        .iter()
        .filter_map(|v| v.as_u64().filter(|&n| n >= 1).map(|n| n as usize))
        .collect();
    if fields.is_empty() {
        return Err(anyhow!("fields must list at least one 1-based field number"));
    }

    let text = String::from_utf8_lossy(&std::fs::read(path)?).into_owned();
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            let parts: Vec<&str> = line.split(delim).collect();
            fields
                .iter()
                .map(|&f| *parts.get(f - 1).unwrap_or(&""))
                .collect::<Vec<_>>()
                .join(out_delim)
        })
        .collect();

    let mut out = json!({ "count": lines.len(), "lines": lines });
    if let Some(output) = opts.get("output").and_then(Value::as_str) {
        let joined = out["lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(output, format!("{joined}\n"))?;
        out["path"] = json!(output);
    }
    Ok(out)
}

/// Wrap long lines to a maximum width (the `fmt`/`fold -s` analogue). opts: path,
/// output (default in place), width => target column width (default 80),
/// break_words => hard-split words longer than `width` (default false: an
/// over-long word stays on its own line intact). Words are split on whitespace
/// and greedily packed; blank lines are preserved. Width is measured in chars.
/// Returns `{ ok, path, lines }` (output line count).
fn op_text_wrap(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let width = opts
        .get("width")
        .and_then(Value::as_u64)
        .filter(|&w| w >= 1)
        .unwrap_or(80) as usize;
    let break_words = opts.get("break_words").and_then(flag_of).unwrap_or(false);

    let text = String::from_utf8_lossy(&std::fs::read(path)?).into_owned();
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in line.split_whitespace() {
            // Hard-break a single word that exceeds the width (when requested).
            let mut word = word.to_string();
            if break_words {
                while word.chars().count() > width {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    let head: String = word.chars().take(width).collect();
                    out.push(head);
                    word = word.chars().skip(width).collect();
                }
            }
            let need = if cur.is_empty() {
                word.chars().count()
            } else {
                cur.chars().count() + 1 + word.chars().count()
            };
            if !cur.is_empty() && need > width {
                out.push(std::mem::take(&mut cur));
            }
            if cur.is_empty() {
                cur = word;
            } else {
                cur.push(' ');
                cur.push_str(&word);
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }

    let joined = format!("{}\n", out.join("\n"));
    std::fs::write(&output, joined)?;
    Ok(json!({ "ok": true, "path": output, "lines": out.len() }))
}
