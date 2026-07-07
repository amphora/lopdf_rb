use lopdf::{Dictionary, Document, Object, ObjectId};
use std::io::Write;

/// Panic-safe stderr diagnostic. `eprintln!` panics if the write to fd 2
/// fails (closed or broken pipe under a supervisor/log shipper); magnus
/// converts a Rust panic into an unrescuable Ruby `fatal`, so a failed log
/// line must never unwind — the write result is deliberately discarded.
fn warn(msg: &str) {
    let _ = writeln!(std::io::stderr(), "lopdf_rb: {}", msg);
}

/// US Letter (width, height) in PDF points — the fallback
/// `get_page_dimensions_or_fallback` applies when no valid MediaBox/CropBox
/// exists. Deliberately not applied inside `get_page_dimensions` itself: the
/// `None` return keeps the boxless case explicit in the type, and the
/// fallback path warns on stderr so the condition is observable in logs
/// (the returned dimensions themselves are identical to a real US Letter
/// page — there is no API-level marker).
pub(crate) const US_LETTER_FALLBACK: (f64, f64) = (612.0, 792.0);

/// Read page dimensions from MediaBox (or CropBox fallback).
/// Returns `Some((width, height))` in PDF points, or `None` when no valid
/// MediaBox or CropBox exists anywhere in the /Parent chain — a malformed
/// bbox (wrong length, non-numeric entries) is treated as absent rather
/// than yielding zero-collapsed dimensions.
/// Handles both direct Array values and indirect References.
///
/// Resolves inherited attributes by walking the /Parent chain (ISO 32000 §7.7.3.4).
/// MediaBox is searched first across the entire chain; CropBox is only used as a
/// fallback if no MediaBox is found anywhere. Capped at 32 levels to guard against
/// circular references in malformed PDFs.
pub(crate) fn get_page_dimensions(doc: &Document, page_id: ObjectId) -> Option<(f64, f64)> {
    find_inherited_bbox(doc, page_id, b"MediaBox")
        .or_else(|| find_inherited_bbox(doc, page_id, b"CropBox"))
}

/// Page dimensions with the shared US Letter fallback applied: returns the
/// real dimensions when a valid MediaBox/CropBox exists, otherwise warns on
/// stderr — naming the 1-based physical page number — and returns
/// `US_LETTER_FALLBACK`. All call sites go through here so the fallback
/// value, warning text, and page-numbering convention stay in one place.
pub(crate) fn get_page_dimensions_or_fallback(
    doc: &Document,
    page_id: ObjectId,
    page_number: u32,
) -> (f64, f64) {
    get_page_dimensions(doc, page_id).unwrap_or_else(|| {
        warn(&format!(
            "page {} has no MediaBox/CropBox; falling back to US Letter ({}x{})",
            page_number, US_LETTER_FALLBACK.0, US_LETTER_FALLBACK.1
        ));
        US_LETTER_FALLBACK
    })
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
                let x1 = obj_to_f64(doc, &bbox[0])?;
                let y1 = obj_to_f64(doc, &bbox[1])?;
                let x2 = obj_to_f64(doc, &bbox[2])?;
                let y2 = obj_to_f64(doc, &bbox[3])?;
                return Some(((x2 - x1).abs(), (y2 - y1).abs()));
            }
        }
    }
    None
}

/// Convert a PDF Object (Integer or Real, possibly behind an indirect
/// reference — ISO 32000 permits references as array elements) to f64.
/// Returns `None` for anything else: the enclosing bbox is malformed, and
/// treating it as absent lets dimension resolution fall through to the
/// /Parent chain, CropBox, and ultimately the callers' US Letter fallback
/// instead of computing stamp coordinates from a zeroed corner. Each
/// rejection emits a stderr diagnostic so the condition stays observable.
pub(crate) fn obj_to_f64(doc: &Document, obj: &Object) -> Option<f64> {
    match obj {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(f) => Some((*f).into()),
        Object::Reference(id) => match doc.get_object(*id) {
            Ok(Object::Integer(i)) => Some(*i as f64),
            Ok(Object::Real(f)) => Some((*f).into()),
            _ => {
                warn(&format!(
                    "bbox entry reference ({} {}) does not resolve to a number; ignoring bbox",
                    id.0, id.1
                ));
                None
            }
        },
        _ => {
            warn(&format!(
                "non-numeric object ({}) in bbox entry; ignoring bbox",
                object_kind(obj)
            ));
            None
        }
    }
}

/// Compact variant label for diagnostics. Never Debug-print a whole
/// untrusted Object: lopdf's Debug impl renders full string/array content
/// with no size cap, so a hostile bbox entry could flood stderr.
fn object_kind(obj: &Object) -> &'static str {
    match obj {
        Object::Null => "Null",
        Object::Boolean(_) => "Boolean",
        Object::Integer(_) => "Integer",
        Object::Real(_) => "Real",
        Object::Name(_) => "Name",
        Object::String(..) => "String",
        Object::Array(_) => "Array",
        Object::Dictionary(_) => "Dictionary",
        Object::Stream(_) => "Stream",
        Object::Reference(_) => "Reference",
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
        let (w, h) = get_page_dimensions(&doc, page_id).unwrap();
        assert!((w - 612.0).abs() < 0.01);
        assert!((h - 792.0).abs() < 0.01);
    }

    #[test]
    fn test_page_dimensions_inherited_from_parent() {
        // No MediaBox on the page, but parent /Pages has one
        let (doc, page_id) = create_test_pdf(None, None, Some(a4_box()));
        let (w, h) = get_page_dimensions(&doc, page_id).unwrap();
        assert!((w - 595.28).abs() < 0.01);
        assert!((h - 841.89).abs() < 0.01);
    }

    #[test]
    fn test_page_dimensions_cropbox_fallback() {
        // No MediaBox anywhere, only CropBox on the page
        let (doc, page_id) = create_test_pdf(None, Some(a4_box()), None);
        let (w, h) = get_page_dimensions(&doc, page_id).unwrap();
        assert!((w - 595.28).abs() < 0.01);
        assert!((h - 841.89).abs() < 0.01);
    }

    #[test]
    fn test_page_dimensions_none_when_no_bbox() {
        // No MediaBox or CropBox anywhere → None; the fallback is the callers' job
        let (doc, page_id) = create_test_pdf(None, None, None);
        assert_eq!(get_page_dimensions(&doc, page_id), None);
    }

    #[test]
    fn test_obj_to_f64_integer_and_real() {
        let doc = Document::new();
        assert_eq!(obj_to_f64(&doc, &Object::Integer(42)), Some(42.0));
        let real = obj_to_f64(&doc, &Object::Real(3.14)).unwrap();
        assert!((real - 3.14).abs() < 0.001);
    }

    #[test]
    fn test_obj_to_f64_non_numeric_is_none() {
        let doc = Document::new();
        assert_eq!(obj_to_f64(&doc, &Object::Boolean(true)), None);
        assert_eq!(obj_to_f64(&doc, &Object::Null), None);
        assert_eq!(obj_to_f64(&doc, &Object::Name(b"Foo".to_vec())), None);
    }

    #[test]
    fn test_obj_to_f64_resolves_indirect_reference() {
        let mut doc = Document::new();
        let id = doc.add_object(Object::Real(612.5));
        assert_eq!(obj_to_f64(&doc, &Object::Reference(id)), Some(612.5));
    }

    #[test]
    fn test_obj_to_f64_unresolvable_reference_is_none() {
        let mut doc = Document::new();
        // Dangling reference, and a reference to a non-numeric object
        assert_eq!(obj_to_f64(&doc, &Object::Reference((9999, 0))), None);
        let id = doc.add_object(Object::Name(b"NotANumber".to_vec()));
        assert_eq!(obj_to_f64(&doc, &Object::Reference(id)), None);
    }

    #[test]
    fn test_page_dimensions_malformed_mediabox_falls_through_to_cropbox() {
        // A MediaBox with a non-numeric entry is treated as absent, so
        // resolution recovers via the valid CropBox instead of returning
        // zero-collapsed dimensions.
        let malformed = vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Null,
            Object::Integer(792),
        ];
        let (doc, page_id) = create_test_pdf(Some(malformed), Some(a4_box()), None);
        let (w, h) = get_page_dimensions(&doc, page_id).unwrap();
        assert!((w - 595.28).abs() < 0.01);
        assert!((h - 841.89).abs() < 0.01);
    }

    #[test]
    fn test_page_dimensions_malformed_bbox_only_is_none() {
        // Only a malformed MediaBox exists → treated as boxless, callers
        // apply the US Letter fallback.
        let malformed = vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Null,
            Object::Integer(792),
        ];
        let (doc, page_id) = create_test_pdf(Some(malformed), None, None);
        assert_eq!(get_page_dimensions(&doc, page_id), None);
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
