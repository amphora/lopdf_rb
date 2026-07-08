use lopdf::{Document, Object, ObjectId, Dictionary, StringFormat};

pub(crate) fn add_invisible_annotation(doc: &mut Document, dlp_data: &str) -> Result<(), String> {
    let pages = doc.get_pages();
    let last_page_id = *pages.values().last()
        .ok_or_else(|| "PDF has no pages — cannot add DLP annotation".to_string())?;

    // Create FreeText annotation dictionary
    let mut annot_dict = Dictionary::new();
    annot_dict.set("Type", Object::Name(b"Annot".to_vec()));
    annot_dict.set("Subtype", Object::Name(b"FreeText".to_vec()));
    annot_dict.set("Rect", Object::Array(vec![
        Object::Real(0.0), Object::Real(0.0),
        Object::Real(1.0), Object::Real(1.0),
    ]));
    annot_dict.set("Contents", Object::String(dlp_data.as_bytes().to_vec(), StringFormat::Literal));
    // F = 34 means Hidden (bit 2 = 2) + NoView (bit 6 = 32)
    annot_dict.set("F", Object::Integer(34));
    annot_dict.set("DA", Object::String(b"/Helv 0 Tf 1 1 1 rg".to_vec(), StringFormat::Literal));

    let annot_id = doc.add_object(annot_dict);

    // Add annotation reference to the page's /Annots array.
    // /Annots can be a direct Array or an indirect Reference to an Array.
    add_to_annots(doc, last_page_id, annot_id);

    Ok(())
}

fn add_to_annots(doc: &mut Document, page_id: ObjectId, annot_id: ObjectId) {
    // First, check if /Annots is an indirect reference and resolve it
    let annots_ref_id = if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
        if let Ok(Object::Reference(ref_id)) = page_dict.get(b"Annots") {
            Some(*ref_id)
        } else {
            None
        }
    } else {
        None
    };

    if let Some(ref_id) = annots_ref_id {
        // /Annots is indirect — append to the referenced array
        if let Ok(Object::Array(ref mut annots)) = doc.get_object_mut(ref_id) {
            annots.push(Object::Reference(annot_id));
            return;
        }
    }

    // /Annots is direct or missing — handle inline
    if let Ok(Object::Dictionary(ref mut page_dict)) = doc.get_object_mut(page_id) {
        match page_dict.get_mut(b"Annots") {
            Ok(Object::Array(ref mut annots)) => {
                annots.push(Object::Reference(annot_id));
            }
            _ => {
                page_dict.set("Annots", Object::Array(vec![Object::Reference(annot_id)]));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Document, Stream};

    fn create_test_pdf(page_count: usize) -> Document {
        let mut doc = Document::new();
        let pages_id = doc.new_object_id();

        let mut kids = Vec::new();
        for _ in 0..page_count {
            let content_stream = Stream::new(Dictionary::new(), Vec::new());
            let content_id = doc.add_object(content_stream);

            let mut page_dict = Dictionary::new();
            page_dict.set("Type", Object::Name(b"Page".to_vec()));
            page_dict.set("Parent", Object::Reference(pages_id));
            page_dict.set("MediaBox", Object::Array(vec![
                Object::Integer(0), Object::Integer(0),
                Object::Integer(612), Object::Integer(792),
            ]));
            page_dict.set("Contents", Object::Reference(content_id));
            let page_id = doc.add_object(page_dict);
            kids.push(Object::Reference(page_id));
        }

        let mut pages_dict = Dictionary::new();
        pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
        pages_dict.set("Kids", Object::Array(kids));
        pages_dict.set("Count", Object::Integer(page_count as i64));
        doc.objects.insert(pages_id, Object::Dictionary(pages_dict));

        let mut catalog = Dictionary::new();
        catalog.set("Type", Object::Name(b"Catalog".to_vec()));
        catalog.set("Pages", Object::Reference(pages_id));
        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", Object::Reference(catalog_id));

        doc
    }

    #[test]
    fn test_add_annotation_creates_freetext() {
        let mut doc = create_test_pdf(1);
        let pages = doc.get_pages();
        let page_id = *pages.values().next().unwrap();

        add_invisible_annotation(&mut doc, r#"{"reader":"Test"}"#).unwrap();

        // Find the annotation on the page
        if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
            if let Ok(Object::Array(ref annots)) = page_dict.get(b"Annots") {
                assert_eq!(annots.len(), 1);
                if let Object::Reference(annot_ref) = &annots[0] {
                    if let Ok(Object::Dictionary(ref annot)) = doc.get_object(*annot_ref) {
                        assert_eq!(annot.get(b"Subtype").unwrap(), &Object::Name(b"FreeText".to_vec()));
                        assert_eq!(annot.get(b"F").unwrap(), &Object::Integer(34));
                        assert_eq!(
                            annot.get(b"Contents").unwrap(),
                            &Object::String(br#"{"reader":"Test"}"#.to_vec(), StringFormat::Literal)
                        );
                        return;
                    }
                }
            }
        }
        panic!("Annotation not found on page");
    }

    #[test]
    fn test_add_annotation_targets_last_page() {
        let mut doc = create_test_pdf(3);
        let pages = doc.get_pages();
        let page_ids: Vec<ObjectId> = pages.iter()
            .collect::<std::collections::BTreeMap<_, _>>()
            .values()
            .map(|id| **id)
            .collect();

        add_invisible_annotation(&mut doc, "dlp-data").unwrap();

        // First two pages should have no annotations
        for &pid in &page_ids[..2] {
            if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(pid) {
                assert!(page_dict.get(b"Annots").is_err(), "Page should have no annotations");
            }
        }

        // Last page should have one annotation
        let last_pid = page_ids[2];
        if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(last_pid) {
            if let Ok(Object::Array(ref annots)) = page_dict.get(b"Annots") {
                assert_eq!(annots.len(), 1);
                return;
            }
        }
        panic!("Annotation not found on last page");
    }

    #[test]
    fn test_add_annotation_appends_to_indirect_annots() {
        let mut doc = create_test_pdf(1);
        let pages = doc.get_pages();
        let page_id = *pages.values().next().unwrap();

        // Pre-existing annotation held in a separately-inserted array that
        // the page references indirectly — the other legal /Annots layout.
        let mut existing_annot = Dictionary::new();
        existing_annot.set("Type", Object::Name(b"Annot".to_vec()));
        existing_annot.set("Subtype", Object::Name(b"Link".to_vec()));
        let existing_id = doc.add_object(existing_annot);
        let array_id = doc.add_object(Object::Array(vec![Object::Reference(existing_id)]));

        if let Ok(Object::Dictionary(ref mut page_dict)) = doc.get_object_mut(page_id) {
            page_dict.set("Annots", Object::Reference(array_id));
        }

        add_invisible_annotation(&mut doc, "dlp-data").unwrap();

        // The referenced array gains the new annotation alongside the existing one
        match doc.get_object(array_id) {
            Ok(Object::Array(ref annots)) => {
                assert_eq!(annots.len(), 2, "Should have 2 annotations (existing + new)");
                assert_eq!(annots[0], Object::Reference(existing_id));
                assert!(matches!(annots[1], Object::Reference(_)), "new entry should be a reference");
            }
            other => panic!("/Annots array not found at its object id: {:?}", other),
        }

        // The page dictionary still holds the indirect reference — not a
        // flattened direct array
        if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
            assert_eq!(
                page_dict.get(b"Annots").unwrap(),
                &Object::Reference(array_id),
                "/Annots should remain an indirect reference"
            );
        } else {
            panic!("Page dictionary not found");
        }
    }

    #[test]
    fn test_add_annotation_appends_to_existing_annots() {
        let mut doc = create_test_pdf(1);
        let pages = doc.get_pages();
        let page_id = *pages.values().next().unwrap();

        // Add a pre-existing annotation
        let mut existing_annot = Dictionary::new();
        existing_annot.set("Type", Object::Name(b"Annot".to_vec()));
        existing_annot.set("Subtype", Object::Name(b"Link".to_vec()));
        let existing_id = doc.add_object(existing_annot);

        if let Ok(Object::Dictionary(ref mut page_dict)) = doc.get_object_mut(page_id) {
            page_dict.set("Annots", Object::Array(vec![Object::Reference(existing_id)]));
        }

        add_invisible_annotation(&mut doc, "dlp-data").unwrap();

        if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
            if let Ok(Object::Array(ref annots)) = page_dict.get(b"Annots") {
                assert_eq!(annots.len(), 2, "Should have 2 annotations (existing + new)");
                return;
            }
        }
        panic!("Annotations not found on page");
    }
}
