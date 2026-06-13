// PDF manipulation (merge / split / rotate / info) via `lopdf`.
//
// These operate on existing PDFs — parsing the object graph, which is why
// they use `lopdf` rather than the from-scratch emitters in `pdf_build` /
// `chart_svg`. The merge routine is a faithful port of lopdf's own
// `examples/merge.rs` (minus the bookmark/outline layering); rotate ports
// `examples/rotate.rs`.

use lopdf::{Document, Object, ObjectId};
use std::collections::BTreeMap;

/// Concatenate several PDFs into one. opts: inputs => [paths], path => output.
fn op_pdf_merge(opts: Value) -> Result<Value> {
    let inputs: Vec<String> = opts
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing inputs (expected array of paths)"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if inputs.is_empty() {
        return Err(anyhow!("no input PDFs to merge"));
    }
    let out = req_str(&opts, "path")?.to_string();

    let mut max_id = 1;
    let mut documents_pages: BTreeMap<ObjectId, Object> = BTreeMap::new();
    let mut documents_objects: BTreeMap<ObjectId, Object> = BTreeMap::new();
    let mut document = Document::with_version("1.5");

    for p in &inputs {
        let mut doc = Document::load(p).map_err(|e| anyhow!("load {p}: {e}"))?;
        doc.renumber_objects_with(max_id);
        max_id = doc.max_id + 1;
        // collect each page object (cloned) before moving the object map
        for object_id in doc.get_pages().into_values() {
            if let Ok(obj) = doc.get_object(object_id) {
                documents_pages.insert(object_id, obj.to_owned());
            }
        }
        documents_objects.extend(doc.objects);
    }

    // Catalog + Pages are mandatory; collect them and pass everything else
    // straight through.
    let mut catalog_object: Option<(ObjectId, Object)> = None;
    let mut pages_object: Option<(ObjectId, Object)> = None;
    for (object_id, object) in documents_objects.into_iter() {
        match object.type_name().unwrap_or(b"") {
            b"Catalog" => {
                catalog_object = Some((catalog_object.map(|(id, _)| id).unwrap_or(object_id), object));
            }
            b"Pages" => {
                if let Ok(dictionary) = object.as_dict() {
                    let mut dictionary = dictionary.clone();
                    if let Some((_, ref old)) = pages_object {
                        if let Ok(old_dict) = old.as_dict() {
                            dictionary.extend(old_dict);
                        }
                    }
                    pages_object = Some((
                        pages_object.map(|(id, _)| id).unwrap_or(object_id),
                        Object::Dictionary(dictionary),
                    ));
                }
            }
            b"Page" | b"Outlines" | b"Outline" => {} // pages handled below; outlines unsupported
            _ => {
                document.objects.insert(object_id, object);
            }
        }
    }

    let Some((pages_id, pages_obj)) = pages_object else {
        return Err(anyhow!("no Pages root found in inputs"));
    };
    let Some((catalog_id, catalog_obj)) = catalog_object else {
        return Err(anyhow!("no Catalog root found in inputs"));
    };

    // Re-parent every page to the merged Pages node.
    for (object_id, object) in documents_pages.iter() {
        if let Ok(dictionary) = object.as_dict() {
            let mut dictionary = dictionary.clone();
            dictionary.set("Parent", pages_id);
            document.objects.insert(*object_id, Object::Dictionary(dictionary));
        }
    }

    // Rebuild Pages with the full Kids list + Count.
    if let Ok(dict) = pages_obj.as_dict() {
        let mut dict = dict.clone();
        dict.set("Count", documents_pages.len() as u32);
        dict.set(
            "Kids",
            documents_pages.keys().map(|id| Object::Reference(*id)).collect::<Vec<_>>(),
        );
        document.objects.insert(pages_id, Object::Dictionary(dict));
    }
    // Rebuild Catalog → Pages.
    if let Ok(dict) = catalog_obj.as_dict() {
        let mut dict = dict.clone();
        dict.set("Pages", pages_id);
        dict.remove(b"Outlines");
        document.objects.insert(catalog_id, Object::Dictionary(dict));
    }
    document.trailer.set("Root", catalog_id);
    document.max_id = document.objects.len() as u32;
    document.renumber_objects();
    document.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "pages": documents_pages.len(), "merged": inputs.len()}))
}

/// Extract a subset of pages into a new PDF. opts: path => input, pages =>
/// [1-based page numbers to keep], output => path. Kept pages stay in their
/// original order.
fn op_pdf_split(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let keep: std::collections::BTreeSet<u32> = opts
        .get("pages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing pages (expected array of 1-based page numbers)"))?
        .iter()
        .filter_map(|v| v.as_u64().map(|n| n as u32))
        .collect();
    if keep.is_empty() {
        return Err(anyhow!("no pages selected"));
    }
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let total = doc.get_pages().len() as u32;
    let remove: Vec<u32> = (1..=total).filter(|p| !keep.contains(p)).collect();
    doc.delete_pages(&remove);
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "pages": (total as usize).saturating_sub(remove.len())}))
}

/// Rotate pages by `angle` degrees (multiple of 90). opts: path => input,
/// angle, output => path, pages => [1-based subset] (default: all).
fn op_pdf_rotate(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let angle = opts.get("angle").and_then(Value::as_i64).unwrap_or(90);
    let subset: Option<std::collections::BTreeSet<u32>> = opts
        .get("pages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as u32)).collect());
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let mut rotated = 0usize;
    for (num, page_id) in doc.get_pages() {
        if let Some(set) = &subset {
            if !set.contains(&num) {
                continue;
            }
        }
        if let Some(dict) = doc.get_object_mut(page_id).ok().and_then(|o| o.as_dict_mut().ok()) {
            let current = dict.get(b"Rotate").and_then(|o| o.as_i64()).unwrap_or(0);
            dict.set("Rotate", (current + angle).rem_euclid(360));
            rotated += 1;
        }
    }
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "rotated": rotated, "angle": angle}))
}

/// A PDF text-string dictionary value as UTF-8 (lossy).
fn pdf_dict_str(dict: &lopdf::Dictionary, key: &[u8]) -> Option<String> {
    match dict.get(key).ok()? {
        Object::String(bytes, _) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

/// A PDF number (Integer or Real) as f64.
fn obj_num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

/// MediaBox (width, height) of a page, defaulting to A4.
fn pdf_page_size(doc: &Document, page_id: ObjectId) -> (f64, f64) {
    doc.get_object(page_id)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"MediaBox").ok())
        .and_then(|o| o.as_array().ok())
        .and_then(|a| Some((obj_num(a.get(2)?)?, obj_num(a.get(3)?)?)))
        .unwrap_or((595.0, 842.0))
}

/// Add an inline Helvetica font named `name` to a page's Resources.
fn ensure_helvetica(doc: &mut Document, page_id: ObjectId, name: &str) -> Result<()> {
    use lopdf::Dictionary;
    let res = doc.get_or_create_resources(page_id).map_err(|e| anyhow!("resources: {e}"))?;
    let rdict = res.as_dict_mut().map_err(|e| anyhow!("resources dict: {e}"))?;
    if !rdict.has(b"Font") {
        rdict.set("Font", Dictionary::new());
    }
    let fonts = rdict
        .get_mut(b"Font")
        .ok()
        .and_then(|o| o.as_dict_mut().ok())
        .ok_or_else(|| anyhow!("font dict"))?;
    let mut fd = Dictionary::new();
    fd.set("Type", "Font");
    fd.set("Subtype", "Type1");
    fd.set("BaseFont", "Helvetica");
    fonts.set(name, Object::Dictionary(fd));
    Ok(())
}

/// RGB 0..1 of a color value (default light gray for watermarks).
fn pdf_color01(v: Option<&Value>, default: [u8; 3]) -> (f64, f64, f64) {
    let c = match v {
        Some(_) => parse_color(v),
        None => image::Rgba([default[0], default[1], default[2], 255]),
    };
    (c.0[0] as f64 / 255.0, c.0[1] as f64 / 255.0, c.0[2] as f64 / 255.0)
}

/// Stamp a rotated text watermark across every page. opts: path => input,
/// output, text, size (60), color (default light gray), angle (deg, 45).
fn op_pdf_watermark(opts: Value) -> Result<Value> {
    use lopdf::content::{Content, Operation};
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let text = req_str(&opts, "text")?.to_string();
    let size = opts.get("size").and_then(Value::as_f64).unwrap_or(60.0);
    let (r, g, b) = pdf_color01(opts.get("color"), [200, 200, 200]);
    let angle = opts.get("angle").and_then(Value::as_f64).unwrap_or(45.0);
    let (sin, cos) = angle.to_radians().sin_cos();
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let mut stamped = 0usize;
    for (_, page_id) in doc.get_pages() {
        ensure_helvetica(&mut doc, page_id, "SOFW")?;
        let (pw, ph) = pdf_page_size(&doc, page_id);
        let ops = vec![
            Operation::new("q", vec![]),
            Operation::new("BT", vec![]),
            Operation::new("rg", vec![r.into(), g.into(), b.into()]),
            Operation::new("Tf", vec!["SOFW".into(), size.into()]),
            Operation::new("Tm", vec![cos.into(), sin.into(), (-sin).into(), cos.into(), (pw * 0.12).into(), (ph * 0.35).into()]),
            Operation::new("Tj", vec![Object::string_literal(text.clone())]),
            Operation::new("ET", vec![]),
            Operation::new("Q", vec![]),
        ];
        doc.add_to_page_content(page_id, Content { operations: ops }).map_err(|e| anyhow!("stamp page: {e}"))?;
        stamped += 1;
    }
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "stamped": stamped}))
}

/// Add footer page numbers to every page. opts: path => input, output,
/// format ("{n} / {total}"), size (10), color (black), y (24).
fn op_pdf_page_numbers(opts: Value) -> Result<Value> {
    use lopdf::content::{Content, Operation};
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let fmt = opts.get("format").and_then(Value::as_str).unwrap_or("{n} / {total}").to_string();
    let size = opts.get("size").and_then(Value::as_f64).unwrap_or(10.0);
    let (r, g, b) = pdf_color01(opts.get("color"), [40, 40, 40]);
    let y = opts.get("y").and_then(Value::as_f64).unwrap_or(24.0);
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let pages = doc.get_pages();
    let total = pages.len();
    for (num, page_id) in pages {
        ensure_helvetica(&mut doc, page_id, "SOFP")?;
        let (pw, _) = pdf_page_size(&doc, page_id);
        let text = fmt.replace("{n}", &num.to_string()).replace("{total}", &total.to_string());
        let x = (pw / 2.0 - text.len() as f64 * size * 0.25).max(4.0);
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("rg", vec![r.into(), g.into(), b.into()]),
            Operation::new("Tf", vec!["SOFP".into(), size.into()]),
            Operation::new("Td", vec![x.into(), y.into()]),
            Operation::new("Tj", vec![Object::string_literal(text)]),
            Operation::new("ET", vec![]),
        ];
        doc.add_to_page_content(page_id, Content { operations: ops }).map_err(|e| anyhow!("number page: {e}"))?;
    }
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "pages": total}))
}

/// Page count, version, and document-info metadata. opts: path => input.
fn op_pdf_info(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let pages = doc.get_pages().len();
    let mut out = json!({"pages": pages, "version": doc.version});
    // Document info dictionary (Title/Author/...), if present.
    if let Some(info) = doc
        .trailer
        .get(b"Info")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .and_then(|id| doc.get_object(id).ok())
        .and_then(|o| o.as_dict().ok())
    {
        for (json_key, pdf_key) in [
            ("title", b"Title".as_slice()),
            ("author", b"Author"),
            ("subject", b"Subject"),
            ("creator", b"Creator"),
            ("producer", b"Producer"),
            ("keywords", b"Keywords"),
        ] {
            if let Some(v) = pdf_dict_str(info, pdf_key) {
                out[json_key] = Value::String(v);
            }
        }
    }
    Ok(out)
}

// ── security: encrypt / decrypt ───────────────────────────────────────────────

/// Translate permission-name strings into lopdf `Permissions` bits. A missing
/// `permissions` key grants everything; an unknown name is ignored. Names:
/// print, modify, copy, annotate, fill, accessibility, assemble, print_hq.
fn pdf_permissions(opts: &Value) -> lopdf::Permissions {
    use lopdf::Permissions;
    let Some(names) = opts.get("permissions").and_then(Value::as_array) else {
        return Permissions::all();
    };
    let mut perms = Permissions::empty();
    for name in names.iter().filter_map(Value::as_str) {
        perms |= match name {
            "print" => Permissions::PRINTABLE,
            "modify" => Permissions::MODIFIABLE,
            "copy" => Permissions::COPYABLE,
            "annotate" => Permissions::ANNOTABLE,
            "fill" => Permissions::FILLABLE,
            "accessibility" => Permissions::COPYABLE_FOR_ACCESSIBILITY,
            "assemble" => Permissions::ASSEMBLABLE,
            "print_hq" => Permissions::PRINTABLE_IN_HIGH_QUALITY,
            _ => continue,
        };
    }
    perms
}

/// Ensure the trailer carries a file `/ID`. The standard security handler
/// derives its file-encryption key from the ID, so an ID-less PDF (e.g. one of
/// our from-scratch builder outputs) can't be encrypted until one exists. The
/// bytes are a stable hash of `seed` — the PDF spec only asks the ID be a
/// per-file identifier, not cryptographically random; the password supplies the
/// security.
fn ensure_pdf_id(doc: &mut Document, seed: &[u8]) {
    use lopdf::StringFormat;
    use std::hash::{Hash, Hasher};
    if doc.trailer.get(b"ID").is_ok() {
        return;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut h);
    let lo = h.finish();
    (seed, lo).hash(&mut h);
    let hi = h.finish();
    let mut id = Vec::with_capacity(16);
    id.extend_from_slice(&lo.to_le_bytes());
    id.extend_from_slice(&hi.to_le_bytes());
    let s = Object::String(id, StringFormat::Hexadecimal);
    doc.trailer.set("ID", Object::Array(vec![s.clone(), s]));
}

/// Password-protect an existing PDF (standard security handler). opts:
/// path => input, output => path, owner_password (""), user_password (""),
/// aes => bool (AES-128 / V4; default RC4 / V2), key_length (V2 bits, default
/// 128), permissions => [names] (default: all granted). Ports the version
/// selection from lopdf's `examples/encrypt.rs`.
fn op_pdf_encrypt(opts: Value) -> Result<Value> {
    use lopdf::encryption::crypt_filters::{Aes128CryptFilter, CryptFilter};
    use lopdf::{EncryptionState, EncryptionVersion};
    use std::sync::Arc;

    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let owner = opts
        .get("owner_password")
        .and_then(Value::as_str)
        .unwrap_or("");
    let user = opts
        .get("user_password")
        .and_then(Value::as_str)
        .unwrap_or("");
    let aes = opts.get("aes").and_then(Value::as_bool).unwrap_or(false);
    let key_length = opts
        .get("key_length")
        .and_then(Value::as_u64)
        .unwrap_or(128) as usize;
    let permissions = pdf_permissions(&opts);

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    if doc.is_encrypted() {
        return Err(anyhow!("already encrypted"));
    }
    ensure_pdf_id(&mut doc, path.as_bytes());
    let state = if aes {
        let cf: Arc<dyn CryptFilter> = Arc::new(Aes128CryptFilter);
        EncryptionState::try_from(EncryptionVersion::V4 {
            document: &doc,
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), cf)]),
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: owner,
            user_password: user,
            permissions,
        })
        .map_err(|e| anyhow!("build encryption state: {e}"))?
    } else {
        EncryptionState::try_from(EncryptionVersion::V2 {
            document: &doc,
            owner_password: owner,
            user_password: user,
            key_length,
            permissions,
        })
        .map_err(|e| anyhow!("build encryption state: {e}"))?
    };
    doc.encrypt(&state).map_err(|e| anyhow!("encrypt: {e}"))?;
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    let method = if aes {
        "aes-128".to_string()
    } else {
        format!("rc4-{key_length}")
    };
    Ok(json!({"ok": true, "path": out, "method": method}))
}

/// Strip password protection from a PDF. opts: path => input, output => path,
/// password (owner or user; default ""). `load_with_password` authenticates,
/// decrypts every string/stream, and drops the trailer /Encrypt entry, so the
/// saved file is plaintext.
fn op_pdf_decrypt(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let password = opts.get("password").and_then(Value::as_str).unwrap_or("");
    let mut doc =
        Document::load_with_password(path, password).map_err(|e| anyhow!("load {path}: {e}"))?;
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out}))
}

/// Shrink a PDF: drop unreferenced objects, then deflate content into object
/// streams. opts: path => input, output => path. Returns byte sizes before /
/// after and the delta.
fn op_pdf_compress(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let before = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    doc.prune_objects();
    doc.compress();
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    let after = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok(json!({
        "ok": true,
        "path": out,
        "before": before,
        "after": after,
        "saved": before.saturating_sub(after),
    }))
}
