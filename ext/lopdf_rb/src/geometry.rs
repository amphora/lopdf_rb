use lopdf::{Dictionary, Document, Object, ObjectId};

/// Read page dimensions from MediaBox (or CropBox fallback).
/// Returns (width, height) in PDF points.
/// Handles both direct Array values and indirect References.
///
/// Resolves inherited attributes by walking the /Parent chain (ISO 32000 §7.7.3.4).
/// MediaBox is searched first across the entire chain; CropBox is only used as a
/// fallback if no MediaBox is found anywhere. Capped at 32 levels to guard against
/// circular references in malformed PDFs.
pub(crate) fn get_page_dimensions(doc: &Document, page_id: ObjectId) -> (f64, f64) {
    if let Some(dims) = find_inherited_bbox(doc, page_id, b"MediaBox") {
        return dims;
    }
    if let Some(dims) = find_inherited_bbox(doc, page_id, b"CropBox") {
        return dims;
    }
    // Fallback to US Letter
    (612.0, 792.0)
}

/// Walk the /Parent chain looking for a specific bbox key (e.g. MediaBox, CropBox).
fn find_inherited_bbox(doc: &Document, page_id: ObjectId, key: &[u8]) -> Option<(f64, f64)> {
    let mut current_id = page_id;
    for _ in 0..32 {
        if let Ok(Object::Dictionary(ref dict)) = doc.get_object(current_id) {
            if let Some(dims) = extract_bbox(doc, dict, key) {
                return Some(dims);
            }
            if let Ok(Object::Reference(parent_id)) = dict.get(b"Parent") {
                current_id = *parent_id;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    None
}

/// Try to extract a bounding box from a dictionary for the given key.
/// Returns (width, height) if a valid 4-element bbox array is found.
fn extract_bbox(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<(f64, f64)> {
    if let Ok(val) = dict.get(key) {
        let bbox = match val {
            Object::Array(ref a) => Some(a),
            Object::Reference(ref_id) => {
                if let Ok(Object::Array(ref a)) = doc.get_object(*ref_id) {
                    Some(a)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(bbox) = bbox {
            if bbox.len() == 4 {
                let x1 = obj_to_f64(&bbox[0]);
                let y1 = obj_to_f64(&bbox[1]);
                let x2 = obj_to_f64(&bbox[2]);
                let y2 = obj_to_f64(&bbox[3]);
                return Some(((x2 - x1).abs(), (y2 - y1).abs()));
            }
        }
    }
    None
}

/// Convert a PDF Object (Integer or Real) to f64.
pub(crate) fn obj_to_f64(obj: &Object) -> f64 {
    match obj {
        Object::Integer(i) => *i as f64,
        Object::Real(f) => (*f).into(),
        _ => 0.0,
    }
}

/// Resolve x coordinate based on origin.
/// "left" (default): abs_x = x
/// "right": abs_x = page_width - x
/// "centre": abs_x = (page_width / 2) + x
pub(crate) fn resolve_x(x: f64, origin: &str, page_width: f64) -> f64 {
    match origin {
        "right" => page_width - x,
        "centre" | "center" => (page_width / 2.0) + x,
        _ => x, // "left" or default
    }
}

/// Resolve y coordinate based on origin.
/// "bottom" (default): abs_y = y
/// "top": abs_y = page_height - y
/// "middle": abs_y = (page_height / 2) + y
pub(crate) fn resolve_y(y: f64, origin: &str, page_height: f64) -> f64 {
    match origin {
        "top" => page_height - y,
        "middle" => (page_height / 2.0) + y,
        _ => y, // "bottom" or default
    }
}
