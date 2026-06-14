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
