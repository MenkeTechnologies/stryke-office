// Document metadata (core / app properties) — read + write across every
// container format the package handles. This is pure format plumbing, not data
// manipulation (the language stdlib owns that): OOXML keeps properties in
// docProps/core.xml + docProps/app.xml, ODF in meta.xml, PDF in the Info
// dictionary. A single normalized key set maps onto all three.
//
// Canonical keys: title, author, subject, keywords, description, category,
// last_modified_by, app, producer, company, created, modified. `meta_read`
// returns whichever are present; `meta_write` sets whichever are supplied and
// leaves the rest of the file byte-for-byte intact (lossless zip raw-copy /
// in-place Info-dict edit).

// `HashMap` and `io::Write` are already in scope from other include!d modules.
use std::collections::HashSet;

// canonical key -> OOXML docProps/core.xml qualified element name
const OOXML_CORE: &[(&str, &str)] = &[
    ("title", "dc:title"),
    ("subject", "dc:subject"),
    ("author", "dc:creator"),
    ("keywords", "cp:keywords"),
    ("description", "dc:description"),
    ("category", "cp:category"),
    ("last_modified_by", "cp:lastModifiedBy"),
    ("created", "dcterms:created"),
    ("modified", "dcterms:modified"),
];

// canonical key -> ODF meta.xml qualified element name
const ODF_META: &[(&str, &str)] = &[
    ("title", "dc:title"),
    ("subject", "dc:subject"),
    ("author", "dc:creator"),
    ("keywords", "meta:keyword"),
    ("description", "dc:description"),
    ("app", "meta:generator"),
    ("created", "meta:creation-date"),
    ("modified", "dc:date"),
];

// canonical key -> PDF Info dictionary key
const PDF_INFO: &[(&str, &[u8])] = &[
    ("title", b"Title"),
    ("author", b"Author"),
    ("subject", b"Subject"),
    ("keywords", b"Keywords"),
    ("app", b"Creator"),
    ("producer", b"Producer"),
    ("created", b"CreationDate"),
    ("modified", b"ModDate"),
];

fn is_ooxml(ext: &str) -> bool {
    matches!(ext, "xlsx" | "xlsm" | "docx" | "docm" | "pptx" | "pptm")
}
fn is_odf(ext: &str) -> bool {
    matches!(ext, "ods" | "odt" | "odp" | "odg" | "ots" | "ott" | "otp")
}

/// Capture the text content of the first occurrence of each named (qualified)
/// leaf element. Element names are matched exactly as they appear in the XML
/// (e.g. `dc:title`, `Company`).
fn xml_leaf_texts(xml: &[u8], tags: &[&str]) -> HashMap<String, String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut out: HashMap<String, String> = HashMap::new();
    let mut cur: Option<String> = None;
    let mut text = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if cur.is_none() && tags.contains(&name.as_str()) {
                    cur = Some(name);
                    text.clear();
                }
            }
            Ok(Event::Text(e)) => {
                if cur.is_some() {
                    if let Ok(t) = e.xml10_content() {
                        text.push_str(&t);
                    }
                }
            }
            Ok(Event::End(e)) => {
                if let Some(c) = cur.clone() {
                    if String::from_utf8_lossy(e.name().as_ref()) == c {
                        out.entry(c).or_insert_with(|| text.clone());
                        cur = None;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Map a {qualified-tag -> value} map back to canonical keys via `table`.
fn to_canonical(found: &HashMap<String, String>, table: &[(&str, &str)], out: &mut Value) {
    for (canon, tag) in table {
        if let Some(v) = found.get(*tag) {
            if !v.is_empty() {
                out[*canon] = Value::String(v.clone());
            }
        }
    }
}

// ── read ─────────────────────────────────────────────────────────────────────

fn op_meta_read(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let ext = ext_of(path);
    let mut out = json!({});
    if is_ooxml(&ext) {
        let bytes = std::fs::read(path)?;
        if let Ok(core) = read_zip_entry(&bytes, "docProps/core.xml") {
            let tags: Vec<&str> = OOXML_CORE.iter().map(|(_, t)| *t).collect();
            to_canonical(&xml_leaf_texts(&core, &tags), OOXML_CORE, &mut out);
        }
        if let Ok(app) = read_zip_entry(&bytes, "docProps/app.xml") {
            let found = xml_leaf_texts(&app, &["Application", "Company"]);
            if let Some(v) = found.get("Application") {
                if !v.is_empty() {
                    out["app"] = Value::String(v.clone());
                }
            }
            if let Some(v) = found.get("Company") {
                if !v.is_empty() {
                    out["company"] = Value::String(v.clone());
                }
            }
        }
    } else if is_odf(&ext) {
        let bytes = std::fs::read(path)?;
        if let Ok(meta) = read_zip_entry(&bytes, "meta.xml") {
            let tags: Vec<&str> = ODF_META.iter().map(|(_, t)| *t).collect();
            to_canonical(&xml_leaf_texts(&meta, &tags), ODF_META, &mut out);
        }
    } else if ext == "pdf" {
        read_pdf_meta(path, &mut out)?;
    } else {
        return Err(anyhow!("unsupported format for metadata: {ext}"));
    }
    Ok(out)
}

fn read_pdf_meta(path: &str, out: &mut Value) -> Result<()> {
    use lopdf::Document;
    let doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    if let Some(info) = doc
        .trailer
        .get(b"Info")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .and_then(|id| doc.get_object(id).ok())
        .and_then(|o| o.as_dict().ok())
    {
        for (canon, key) in PDF_INFO {
            if let Some(v) = pdf_dict_str(info, key) {
                if !v.is_empty() {
                    out[*canon] = Value::String(v);
                }
            }
        }
    }
    Ok(())
}

// ── write ──────────────────────────────────────────────────────────────────

/// String value of a supplied prop, if present and a string/number.
fn prop_str(props: &Value, key: &str) -> Option<String> {
    match props.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn op_meta_write(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let props = opts
        .get("props")
        .or_else(|| opts.get("meta"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !props.is_object() {
        return Err(anyhow!("props must be an object of metadata keys"));
    }
    let ext = ext_of(path);
    let mut set = Vec::new();
    if is_ooxml(&ext) {
        write_ooxml_meta(path, &output, &props, &mut set)?;
    } else if is_odf(&ext) {
        write_odf_meta(path, &output, &props, &mut set)?;
    } else if ext == "pdf" {
        write_pdf_meta(path, &output, &props, &mut set)?;
    } else {
        return Err(anyhow!("unsupported format for metadata: {ext}"));
    }
    Ok(json!({"ok": true, "path": output, "set": set}))
}

/// Build a fresh docProps/core.xml from the merged property map. core.xml only
/// ever holds these leaf elements, so a full rebuild is lossless.
fn build_core_xml(merged: &HashMap<String, String>) -> Vec<u8> {
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:dcmitype="http://purl.org/dc/dcmitype/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">"#,
    );
    for (canon, tag) in OOXML_CORE {
        if let Some(v) = merged.get(*canon) {
            let esc = xml_escape(v);
            if *canon == "created" || *canon == "modified" {
                s.push_str(&format!(
                    "<{tag} xsi:type=\"dcterms:W3CDTF\">{esc}</{tag}>"
                ));
            } else {
                s.push_str(&format!("<{tag}>{esc}</{tag}>"));
            }
        }
    }
    s.push_str("</cp:coreProperties>");
    s.into_bytes()
}

fn write_ooxml_meta(path: &str, output: &str, props: &Value, set: &mut Vec<String>) -> Result<()> {
    let bytes = std::fs::read(path)?;
    // Merge existing core values with the supplied ones.
    let mut merged: HashMap<String, String> = HashMap::new();
    if let Ok(core) = read_zip_entry(&bytes, "docProps/core.xml") {
        let tags: Vec<&str> = OOXML_CORE.iter().map(|(_, t)| *t).collect();
        let found = xml_leaf_texts(&core, &tags);
        for (canon, tag) in OOXML_CORE {
            if let Some(v) = found.get(*tag) {
                merged.insert((*canon).to_string(), v.clone());
            }
        }
    }
    let mut touched_core = false;
    for (canon, _) in OOXML_CORE {
        if let Some(v) = prop_str(props, canon) {
            merged.insert((*canon).to_string(), v);
            set.push((*canon).to_string());
            touched_core = true;
        }
    }

    let mut replace: HashMap<String, Vec<u8>> = HashMap::new();
    if touched_core {
        replace.insert("docProps/core.xml".to_string(), build_core_xml(&merged));
    }

    // app.xml is patched in place (it holds other elements we must preserve);
    // only when app / company were supplied.
    let want_app = prop_str(props, "app");
    let want_company = prop_str(props, "company");
    if want_app.is_some() || want_company.is_some() {
        let app_xml = read_zip_entry(&bytes, "docProps/app.xml")
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_else(|_| String::from(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"></Properties>"#,
            ));
        let mut app_xml = app_xml;
        if let Some(v) = want_app {
            set_xml_leaf(&mut app_xml, "Application", &v, "</Properties>");
            set.push("app".into());
        }
        if let Some(v) = want_company {
            set_xml_leaf(&mut app_xml, "Company", &v, "</Properties>");
            set.push("company".into());
        }
        replace.insert("docProps/app.xml".to_string(), app_xml.into_bytes());
    }

    if replace.is_empty() {
        return Err(anyhow!("no recognized metadata keys supplied"));
    }
    let need_inject = touched_core; // core.xml may be a new part
    let new_bytes = rewrite_zip(&bytes, &replace, need_inject)?;
    std::fs::write(output, new_bytes)?;
    Ok(())
}

/// Replace (or insert) a non-namespaced leaf element `<tag>..</tag>` in `xml`.
/// If absent, insert before `before`. Handles an empty self-closed form too.
fn set_xml_leaf(xml: &mut String, tag: &str, value: &str, before: &str) {
    let esc = xml_escape(value);
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    if let Some(o) = xml.find(&open) {
        if let Some(c) = xml[o..].find(&close) {
            let inner_start = o + open.len();
            let inner_end = o + c;
            xml.replace_range(inner_start..inner_end, &esc);
            return;
        }
    }
    let self_closed = format!("<{tag}/>");
    let repl = format!("<{tag}>{esc}</{tag}>");
    if let Some(p) = xml.find(&self_closed) {
        xml.replace_range(p..p + self_closed.len(), &repl);
        return;
    }
    if let Some(p) = xml.rfind(before) {
        xml.insert_str(p, &repl);
    }
}

/// Lossless zip rewrite: every entry is raw-copied except those in `replace`,
/// which are re-deflated with new content. When `inject_core` is set and
/// docProps/core.xml is new to the package, the content-types override and the
/// package relationship are patched in so the part is actually recognized.
fn rewrite_zip(bytes: &[u8], replace: &HashMap<String, Vec<u8>>, inject_core: bool) -> Result<Vec<u8>> {
    let mut src = zip::ZipArchive::new(Cursor::new(bytes))?;
    let present: HashSet<String> = (0..src.len())
        .filter_map(|i| src.by_index_raw(i).ok().map(|f| f.name().to_string()))
        .collect();

    // Decide whether content-types / rels need patching for a brand-new core part.
    let mut extra: HashMap<String, Vec<u8>> = HashMap::new();
    if inject_core && replace.contains_key("docProps/core.xml") && !present.contains("docProps/core.xml")
    {
        if let Ok(ct) = read_zip_entry(bytes, "[Content_Types].xml") {
            let mut ct = String::from_utf8_lossy(&ct).into_owned();
            if !ct.contains("docProps/core.xml") {
                let ov = r#"<Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>"#;
                if let Some(p) = ct.rfind("</Types>") {
                    ct.insert_str(p, ov);
                }
                extra.insert("[Content_Types].xml".to_string(), ct.into_bytes());
            }
        }
        if let Ok(rels) = read_zip_entry(bytes, "_rels/.rels") {
            let mut rels = String::from_utf8_lossy(&rels).into_owned();
            if !rels.contains("docProps/core.xml") {
                let rel = r#"<Relationship Id="rIdMeta" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>"#;
                if let Some(p) = rels.rfind("</Relationships>") {
                    rels.insert_str(p, rel);
                }
                extra.insert("_rels/.rels".to_string(), rels.into_bytes());
            }
        }
    }

    let opt: zip::write::FileOptions<()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for i in 0..src.len() {
        let f = src.by_index_raw(i)?;
        let name = f.name().to_string();
        if let Some(data) = replace.get(&name).or_else(|| extra.get(&name)) {
            zw.start_file(&name, opt)?;
            zw.write_all(data)?;
        } else {
            zw.raw_copy_file(f)?;
        }
    }
    // Append any parts that did not exist before (new core.xml, etc.).
    for (name, data) in replace.iter().chain(extra.iter()) {
        if !present.contains(name) {
            zw.start_file(name, opt)?;
            zw.write_all(data)?;
        }
    }
    Ok(zw.finish()?.into_inner())
}

/// ODF meta.xml is also a flat property list; rebuild it from merged values.
fn write_odf_meta(path: &str, output: &str, props: &Value, set: &mut Vec<String>) -> Result<()> {
    let bytes = std::fs::read(path)?;
    let mut merged: HashMap<String, String> = HashMap::new();
    if let Ok(meta) = read_zip_entry(&bytes, "meta.xml") {
        let tags: Vec<&str> = ODF_META.iter().map(|(_, t)| *t).collect();
        let found = xml_leaf_texts(&meta, &tags);
        for (canon, tag) in ODF_META {
            if let Some(v) = found.get(*tag) {
                merged.insert((*canon).to_string(), v.clone());
            }
        }
    }
    let mut touched = false;
    for (canon, _) in ODF_META {
        if let Some(v) = prop_str(props, canon) {
            merged.insert((*canon).to_string(), v);
            set.push((*canon).to_string());
            touched = true;
        }
    }
    if !touched {
        return Err(anyhow!("no recognized metadata keys supplied"));
    }
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/"><office:meta>"#,
    );
    for (canon, tag) in ODF_META {
        if let Some(v) = merged.get(*canon) {
            s.push_str(&format!("<{tag}>{}</{tag}>", xml_escape(v)));
        }
    }
    s.push_str("</office:meta></office:document-meta>");
    let mut replace: HashMap<String, Vec<u8>> = HashMap::new();
    replace.insert("meta.xml".to_string(), s.into_bytes());
    let new_bytes = rewrite_zip(&bytes, &replace, false)?;
    // ODF lists parts in META-INF/manifest.xml; ensure meta.xml is declared.
    let new_bytes = ensure_odf_manifest(new_bytes)?;
    std::fs::write(output, new_bytes)?;
    Ok(())
}

/// Make sure META-INF/manifest.xml declares meta.xml (LibreOffice files always
/// do; minimally-built ones may not).
fn ensure_odf_manifest(bytes: Vec<u8>) -> Result<Vec<u8>> {
    let manifest = match read_zip_entry(&bytes, "META-INF/manifest.xml") {
        Ok(m) => String::from_utf8_lossy(&m).into_owned(),
        Err(_) => return Ok(bytes),
    };
    if manifest.contains("full-path=\"meta.xml\"") {
        return Ok(bytes);
    }
    let mut m = manifest;
    let entry = r#"<manifest:file-entry manifest:full-path="meta.xml" manifest:media-type="text/xml"/>"#;
    if let Some(p) = m.rfind("</manifest:manifest>") {
        m.insert_str(p, entry);
    }
    let mut replace: HashMap<String, Vec<u8>> = HashMap::new();
    replace.insert("META-INF/manifest.xml".to_string(), m.into_bytes());
    rewrite_zip(&bytes, &replace, false)
}

/// Normalize an ISO-8601 datetime to a PDF date string `D:YYYYMMDDHHmmSS`.
/// Non-ISO values pass through unchanged.
fn to_pdf_date(s: &str) -> String {
    if s.starts_with("D:") {
        return s.to_string();
    }
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() >= 8 {
        format!("D:{digits}")
    } else {
        s.to_string()
    }
}

fn write_pdf_meta(path: &str, output: &str, props: &Value, set: &mut Vec<String>) -> Result<()> {
    use lopdf::{Dictionary, Document, Object};
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let info_id = match doc
        .trailer
        .get(b"Info")
        .ok()
        .and_then(|o| o.as_reference().ok())
    {
        Some(id) => id,
        None => {
            let id = doc.add_object(Object::Dictionary(Dictionary::new()));
            doc.trailer.set("Info", Object::Reference(id));
            id
        }
    };
    let dict = doc
        .get_object_mut(info_id)
        .ok()
        .and_then(|o| o.as_dict_mut().ok())
        .ok_or_else(|| anyhow!("info dict not a dictionary"))?;
    for (canon, key) in PDF_INFO {
        if let Some(mut v) = prop_str(props, canon) {
            if *canon == "created" || *canon == "modified" {
                v = to_pdf_date(&v);
            }
            dict.set(*key, Object::string_literal(v));
            set.push((*canon).to_string());
        }
    }
    if set.is_empty() {
        return Err(anyhow!("no recognized metadata keys supplied"));
    }
    doc.save(output).map_err(|e| anyhow!("save {output}: {e}"))?;
    Ok(())
}
