// PDF AcroForm fields — list + fill. This is the classic "fill a government /
// business PDF form" capability: enumerate the interactive fields, then set
// their values. Filling sets each field's /V (and a checkbox's /AS) and flips
// the AcroForm /NeedAppearances flag so conformant viewers regenerate the
// on-screen appearance. Pure format-internal object-graph editing via `lopdf`.
//
// `Document`, `Object`, `ObjectId` are already in scope from the pdf_ops
// include; everything else is referenced fully-qualified to avoid clashes.

/// Decode a field value object to a display string (text value, button state,
/// or joined multi-select), following one indirect reference.
fn pdf_value_string(doc: &Document, obj: &Object) -> String {
    let resolved = match obj {
        Object::Reference(id) => doc.get_object(*id).unwrap_or(obj),
        other => other,
    };
    match resolved {
        Object::String(b, _) => String::from_utf8_lossy(b).into_owned(),
        Object::Name(n) => String::from_utf8_lossy(n).into_owned(),
        Object::Array(a) => a
            .iter()
            .map(|o| pdf_value_string(doc, o))
            .collect::<Vec<_>>()
            .join(", "),
        _ => String::new(),
    }
}

/// Field type from /FT, walking the /Parent chain since it can be inherited.
fn pdf_field_type(doc: &Document, dict: &lopdf::Dictionary) -> String {
    let mut cur = dict.clone();
    for _ in 0..16 {
        if let Ok(ft) = cur.get(b"FT").and_then(|o| o.as_name()) {
            return match ft {
                b"Tx" => "text",
                b"Btn" => "button",
                b"Ch" => "choice",
                b"Sig" => "signature",
                _ => "unknown",
            }
            .to_string();
        }
        let Ok(parent) = cur.get(b"Parent").and_then(|o| o.as_reference()) else {
            break;
        };
        let Ok(pd) = doc.get_dictionary(parent) else {
            break;
        };
        cur = pd.clone();
    }
    String::new()
}

/// Collect leaf, named form fields as (full_name, object_id). A node with
/// `/Kids` whose kids carry their own `/T` is a container (recurse); kids
/// without `/T` are just the widget annotations of this field.
fn collect_fields(doc: &Document, ids: &[ObjectId], prefix: &str, out: &mut Vec<(String, ObjectId)>) {
    for &id in ids {
        let Ok(dict) = doc.get_dictionary(id) else {
            continue;
        };
        let t = dict
            .get(b"T")
            .and_then(|o| o.as_str())
            .ok()
            .map(|b| String::from_utf8_lossy(b).into_owned());
        let name = match &t {
            Some(t) if prefix.is_empty() => t.clone(),
            Some(t) => format!("{prefix}.{t}"),
            None => prefix.to_string(),
        };
        let kids: Vec<ObjectId> = dict
            .get(b"Kids")
            .and_then(|o| o.as_array())
            .map(|a| a.iter().filter_map(|o| o.as_reference().ok()).collect())
            .unwrap_or_default();
        let kids_named = kids
            .iter()
            .any(|&k| doc.get_dictionary(k).map(|d| d.has(b"T")).unwrap_or(false));
        if !kids.is_empty() && kids_named {
            collect_fields(doc, &kids, &name, out);
        } else if t.is_some() {
            out.push((name, id));
        }
    }
}

/// The AcroForm dictionary's object id (when it is an indirect object) and the
/// top-level /Fields object ids.
fn acroform(doc: &Document) -> Option<(Option<ObjectId>, Vec<ObjectId>)> {
    let cat = doc.catalog().ok()?;
    let af_obj = cat.get(b"AcroForm").ok()?;
    let (af_id, af_dict) = match af_obj {
        Object::Reference(id) => (Some(*id), doc.get_dictionary(*id).ok()?),
        Object::Dictionary(d) => (None, d),
        _ => return None,
    };
    let fields = af_dict
        .get(b"Fields")
        .and_then(|o| o.as_array())
        .map(|a| a.iter().filter_map(|o| o.as_reference().ok()).collect())
        .unwrap_or_default();
    Some((af_id, fields))
}

fn op_pdf_form_fields(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let Some((_af_id, field_ids)) = acroform(&doc) else {
        return Ok(json!({"fields": [], "count": 0}));
    };
    let mut leaves = Vec::new();
    collect_fields(&doc, &field_ids, "", &mut leaves);

    let mut fields = Vec::new();
    for (name, id) in &leaves {
        let Ok(dict) = doc.get_dictionary(*id) else {
            continue;
        };
        let ftype = pdf_field_type(&doc, dict);
        let value = dict.get(b"V").map(|v| pdf_value_string(&doc, v)).unwrap_or_default();
        let mut entry = json!({"name": name, "type": ftype, "value": value});
        // choice options
        if let Ok(opt) = dict.get(b"Opt").and_then(|o| o.as_array()) {
            let options: Vec<String> = opt.iter().map(|o| pdf_value_string(&doc, o)).collect();
            entry["options"] = json!(options);
        }
        fields.push(entry);
    }
    let count = fields.len();
    Ok(json!({"fields": fields, "count": count}))
}

/// On-state name of a checkbox/radio widget: the first key of /AP /N that is
/// not "Off". Defaults to "Yes".
fn checkbox_on_state(dict: &lopdf::Dictionary) -> String {
    dict.get(b"AP")
        .and_then(|o| o.as_dict())
        .and_then(|ap| ap.get(b"N").and_then(|o| o.as_dict()))
        .ok()
        .and_then(|n| {
            n.iter()
                .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
                .find(|k| k != "Off")
        })
        .unwrap_or_else(|| "Yes".to_string())
}

fn op_pdf_fill_form(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let values = opts
        .get("values")
        .or_else(|| opts.get("fields"))
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("missing values (expected object of field => value)"))?
        .clone();

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let Some((af_id, field_ids)) = acroform(&doc) else {
        return Err(anyhow!("no AcroForm in {path}"));
    };
    let mut leaves = Vec::new();
    collect_fields(&doc, &field_ids, "", &mut leaves);

    // Precompute each target field's type + checkbox on-state before mutating.
    let mut plan: Vec<(ObjectId, String, Object)> = Vec::new();
    for (name, id) in &leaves {
        let Some(val) = values.get(name) else { continue };
        let Ok(dict) = doc.get_dictionary(*id) else {
            continue;
        };
        let ftype = pdf_field_type(&doc, dict);
        let new_v: Object = if ftype == "button" {
            let on = checkbox_on_state(dict);
            let state = match val {
                Value::Bool(true) => on,
                Value::Bool(false) => "Off".to_string(),
                Value::String(s) => s.clone(),
                other => cell_to_string(other),
            };
            Object::Name(state.into_bytes())
        } else {
            Object::string_literal(cell_to_string(val))
        };
        plan.push((*id, ftype, new_v));
    }

    let filled = plan.len();
    for (id, ftype, new_v) in plan {
        if let Ok(dict) = doc.get_dictionary_mut(id) {
            dict.set("V", new_v.clone());
            if ftype == "button" {
                dict.set("AS", new_v);
            }
        }
    }
    // Tell viewers to regenerate field appearances.
    if let Some(af_id) = af_id {
        if let Ok(af) = doc.get_dictionary_mut(af_id) {
            af.set("NeedAppearances", true);
        }
    }
    doc.save(&output).map_err(|e| anyhow!("save {output}: {e}"))?;
    Ok(json!({"ok": true, "path": output, "filled": filled}))
}
