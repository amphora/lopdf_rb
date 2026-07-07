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

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::Stream;

    /// Build a minimal PDF document programmatically for testing.
    /// If `media_box` is Some, sets /MediaBox on the page dict.
    /// If `crop_box` is Some, sets /CropBox on the page dict.
    /// If `parent_media_box` is Some, sets /MediaBox on the parent /Pages dict
    /// (and omits it from the page dict, to test inheritance).
    fn create_test_pdf(
        media_box: Option<Vec<Object>>,
        crop_box: Option<Vec<Object>>,
        parent_media_box: Option<Vec<Object>>,
    ) -> (Document, ObjectId) {
        let mut doc = Document::new();

        // Create a minimal content stream (empty page)
        let content_stream = Stream::new(Dictionary::new(), Vec::new());
        let content_id = doc.add_object(content_stream);

        // Build the /Pages (parent) dictionary
        let pages_id = doc.new_object_id();

        // Build the page dictionary
        let mut page_dict = Dictionary::new();
        page_dict.set("Type", Object::Name(b"Page".to_vec()));
        page_dict.set("Parent", Object::Reference(pages_id));
        page_dict.set("Contents", Object::Reference(content_id));

        if let Some(mb) = media_box {
            page_dict.set("MediaBox", Object::Array(mb));
        }
        if let Some(cb) = crop_box {
            page_dict.set("CropBox", Object::Array(cb));
        }

        let page_id = doc.add_object(page_dict);

        // Build the parent /Pages dictionary
        let mut pages_dict = Dictionary::new();
        pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
        pages_dict.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
        pages_dict.set("Count", Object::Integer(1));

        if let Some(pmb) = parent_media_box {
            pages_dict.set("MediaBox", Object::Array(pmb));
        }

        doc.objects.insert(pages_id, Object::Dictionary(pages_dict));

        // Set the catalog
        let mut catalog = Dictionary::new();
        catalog.set("Type", Object::Name(b"Catalog".to_vec()));
        catalog.set("Pages", Object::Reference(pages_id));
        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", Object::Reference(catalog_id));

        (doc, page_id)
    }

    fn us_letter_box() -> Vec<Object> {
        vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(612),
            Object::Integer(792),
        ]
    }

    fn a4_box() -> Vec<Object> {
        vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Real(595.28),
            Object::Real(841.89),
        ]
    }

    #[test]
    fn test_page_dimensions_direct_mediabox() {
        let (doc, page_id) = create_test_pdf(Some(us_letter_box()), None, None);
        let (w, h) = get_page_dimensions(&doc, page_id);
        assert!((w - 612.0).abs() < 0.01);
        assert!((h - 792.0).abs() < 0.01);
    }

    #[test]
    fn test_page_dimensions_inherited_from_parent() {
        // No MediaBox on the page, but parent /Pages has one
        let (doc, page_id) = create_test_pdf(None, None, Some(a4_box()));
        let (w, h) = get_page_dimensions(&doc, page_id);
        assert!((w - 595.28).abs() < 0.01);
        assert!((h - 841.89).abs() < 0.01);
    }

    #[test]
    fn test_page_dimensions_cropbox_fallback() {
        // No MediaBox anywhere, only CropBox on the page
        let (doc, page_id) = create_test_pdf(None, Some(a4_box()), None);
        let (w, h) = get_page_dimensions(&doc, page_id);
        assert!((w - 595.28).abs() < 0.01);
        assert!((h - 841.89).abs() < 0.01);
    }

    #[test]
    fn test_page_dimensions_default_us_letter() {
        // No MediaBox or CropBox anywhere → falls back to US Letter
        let (doc, page_id) = create_test_pdf(None, None, None);
        let (w, h) = get_page_dimensions(&doc, page_id);
        assert!((w - 612.0).abs() < 0.01);
        assert!((h - 792.0).abs() < 0.01);
    }

    #[test]
    fn test_obj_to_f64_integer_and_real() {
        assert!((obj_to_f64(&Object::Integer(42)) - 42.0).abs() < f64::EPSILON);
        assert!((obj_to_f64(&Object::Real(3.14)) - 3.14).abs() < 0.001);
    }

    #[test]
    fn test_obj_to_f64_unsupported() {
        assert!((obj_to_f64(&Object::Boolean(true))).abs() < f64::EPSILON);
        assert!((obj_to_f64(&Object::Null)).abs() < f64::EPSILON);
        assert!((obj_to_f64(&Object::Name(b"Foo".to_vec()))).abs() < f64::EPSILON);
    }

    #[test]
    fn test_resolve_x_origins() {
        assert!((resolve_x(10.0, "left", 612.0) - 10.0).abs() < f64::EPSILON);
        assert!((resolve_x(10.0, "right", 612.0) - 602.0).abs() < f64::EPSILON);
        assert!((resolve_x(10.0, "centre", 612.0) - 316.0).abs() < f64::EPSILON);
        assert!((resolve_x(10.0, "center", 612.0) - 316.0).abs() < f64::EPSILON);
        // Unknown origin falls through to the "left"/default arm
        assert!((resolve_x(10.0, "bogus", 612.0) - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_resolve_y_origins() {
        assert!((resolve_y(20.0, "bottom", 792.0) - 20.0).abs() < f64::EPSILON);
        assert!((resolve_y(20.0, "top", 792.0) - 772.0).abs() < f64::EPSILON);
        assert!((resolve_y(20.0, "middle", 792.0) - 416.0).abs() < f64::EPSILON);
        // Unknown origin falls through to the "bottom"/default arm
        assert!((resolve_y(20.0, "bogus", 792.0) - 20.0).abs() < f64::EPSILON);
    }
}
