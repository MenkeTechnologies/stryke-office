// PDF file attachments (embedded files) via `lopdf` — embed arbitrary files
// inside a PDF and list/extract them. Built on the catalog name tree
// `/Names /EmbeddedFiles` (PDF spec 7.11): each entry pairs a UTF-8 name with a
// `Filespec` dict whose `/EF /F` points at an `EmbeddedFile` stream.

use lopdf::{Dictionary, Stream};

/// Follow a value that may be an inline dictionary or an indirect reference to
/// one, returning the dictionary.
fn deref_dict<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Dictionary> {
    match o {
        Object::Dictionary(d) => Some(d),
        Object::Reference(id) => doc.get_dictionary(*id).ok(),
        _ => None,
    }
}

/// The flat `[name, filespec, …]` array under `/Names /EmbeddedFiles /Names`,
/// plus a clone of the catalog name dictionary (so its other keys — `Dests`,
/// `JavaScript`, … — are preserved when we rewrite it).
fn embedded_names(doc: &Document) -> (Vec<Object>, Option<Dictionary>) {
    let names_dict = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Names").ok())
        .and_then(|o| deref_dict(doc, o))
        .cloned();
    let pairs = names_dict
        .as_ref()
        .and_then(|nd| nd.get(b"EmbeddedFiles").ok())
        .and_then(|o| deref_dict(doc, o))
        .and_then(|ef| ef.get(b"Names").ok())
        .and_then(|o| o.as_array().ok())
        .cloned()
        .unwrap_or_default();
    (pairs, names_dict)
}

/// Decode every embedded file to `(name, bytes)`.
fn read_embedded_files(doc: &Document) -> Vec<(String, Vec<u8>)> {
    let (pairs, _) = embedded_names(doc);
    let mut out = Vec::new();
    for pair in pairs.chunks(2) {
        let name = pair
            .first()
            .and_then(|o| o.as_str().ok())
            .map(|b| String::from_utf8_lossy(b).into_owned());
        let bytes = pair
            .get(1)
            .and_then(|o| deref_dict(doc, o)) // Filespec
            .and_then(|spec| spec.get(b"EF").ok())
            .and_then(|o| deref_dict(doc, o)) // /EF dict
            .and_then(|ef| ef.get(b"F").ok())
            .and_then(|o| match o {
                Object::Reference(id) => doc.get_object(*id).ok(),
                other => Some(other),
            })
            .and_then(|o| o.as_stream().ok())
            // Uncompressed embedded streams have no /Filter, so decompressed_content
            // errors — fall back to the raw bytes in that case.
            .map(|s| s.decompressed_content().unwrap_or_else(|_| s.content.clone()));
        if let (Some(n), Some(b)) = (name, bytes) {
            out.push((n, b));
        }
    }
    out
}

/// Embed a file inside a PDF. opts: path => input, output => path,
/// file => path to embed, name => stored name (default: the file's basename).
/// Appends to any existing attachments. Returns `{ ok, path, name, size,
/// count }`.
fn op_pdf_attach(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let file = req_str(&opts, "file")?;
    let data = std::fs::read(file)?;
    let size = data.len();
    let name = opts
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            Path::new(file)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("attachment")
                .to_string()
        });

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;

    // Read existing entries first (immutable), then mutate.
    let (existing, names_dict) = embedded_names(&doc);

    // EmbeddedFile stream + Filespec dict.
    let mut params = Dictionary::new();
    params.set("Size", size as i64);
    let mut ef_dict = Dictionary::new();
    ef_dict.set("Type", "EmbeddedFile");
    ef_dict.set("Params", params);
    let ef_id = doc.add_object(Stream::new(ef_dict, data));

    let mut ef_ref = Dictionary::new();
    ef_ref.set("F", Object::Reference(ef_id));
    let mut spec = Dictionary::new();
    spec.set("Type", "Filespec");
    spec.set("F", Object::string_literal(name.clone()));
    spec.set("UF", Object::string_literal(name.clone()));
    spec.set("EF", ef_ref);
    let spec_id = doc.add_object(spec);

    // Merge into the existing name list and re-sort (name trees must be sorted).
    let mut tuples: Vec<(Vec<u8>, Object)> = existing
        .chunks(2)
        .filter_map(|c| Some((c.first()?.as_str().ok()?.to_vec(), c.get(1)?.clone())))
        .collect();
    tuples.push((name.clone().into_bytes(), Object::Reference(spec_id)));
    tuples.sort_by(|a, b| a.0.cmp(&b.0));
    let count = tuples.len();
    let flat: Vec<Object> = tuples
        .into_iter()
        .flat_map(|(n, v)| [Object::string_literal(n), v])
        .collect();

    let mut ef_tree = Dictionary::new();
    ef_tree.set("Names", Object::Array(flat));
    let ef_tree_id = doc.add_object(ef_tree);

    // Rewrite the catalog name dictionary, preserving any other keys it had.
    let mut nd = names_dict.unwrap_or_default();
    nd.set("EmbeddedFiles", Object::Reference(ef_tree_id));
    let nd_id = doc.add_object(nd);
    doc.catalog_mut()
        .map_err(|e| anyhow!("no catalog: {e}"))?
        .set("Names", Object::Reference(nd_id));

    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "name": name, "size": size, "count": count}))
}

/// List (and optionally extract) the files embedded in a PDF. opts: path,
/// extract_dir => write each attachment there (by basename). Returns
/// `{ attachments: [{ name, size }], count }`.
fn op_pdf_attachments(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let files = read_embedded_files(&doc);
    let extract_dir = opts.get("extract_dir").and_then(Value::as_str);
    let mut list = Vec::new();
    for (name, bytes) in &files {
        if let Some(dir) = extract_dir {
            let base = Path::new(name)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(name.as_str());
            std::fs::write(Path::new(dir).join(base), bytes)?;
        }
        list.push(json!({ "name": name, "size": bytes.len() }));
    }
    Ok(json!({ "attachments": list, "count": files.len() }))
}
