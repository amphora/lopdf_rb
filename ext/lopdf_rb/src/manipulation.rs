use lopdf::{Document, Object, ObjectId, Dictionary};
use std::collections::{BTreeMap, HashSet};
use std::io::Cursor;

/// Rotate all pages in the document by the given angle (clockwise).
///
/// Angle must be 0, 90, 180, or 270. Rotation is cumulative — if a page
/// already has `/Rotate 90` and you call `rotate_all_pages(doc, 180)`,
/// the result is `/Rotate 270`.
pub(crate) fn rotate_all_pages(doc: &mut Document, angle: i64) -> Result<(), String> {
    if ![0, 90, 180, 270].contains(&angle) {
        return Err(format!(
            "Invalid rotation angle {}. Must be 0, 90, 180, or 270.",
            angle
        ));
    }

    let page_ids: Vec<ObjectId> = doc.get_pages().values().copied().collect();

    for page_id in page_ids {
        let existing = if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
            match page_dict.get(b"Rotate") {
                Ok(Object::Integer(r)) => *r,
                _ => 0,
            }
        } else {
            0
        };

        let new_angle = ((existing + angle) % 360 + 360) % 360;

        if let Ok(Object::Dictionary(ref mut page_dict)) = doc.get_object_mut(page_id) {
            page_dict.set("Rotate", Object::Integer(new_angle));
        }
    }

    Ok(())
}

/// Split a document into individual single-page documents.
///
/// Uses a sequential clone-and-delete strategy: for each page, clone the
/// entire document, delete all other pages, then prune unreferenced objects.
/// Only one extra copy exists at a time, so peak memory is ~2x source.
pub(crate) fn split_pages(doc: &Document) -> Result<Vec<Document>, String> {
    let page_count = doc.get_pages().len();
    if page_count == 0 {
        return Err("Cannot split a document with no pages".to_string());
    }

    let mut results = Vec::with_capacity(page_count);

    for page_num in 1..=page_count {
        let mut clone = doc.clone();

        // Build list of page numbers to delete (all except current)
        let pages_to_delete: Vec<u32> = (1..=page_count as u32)
            .filter(|&p| p != page_num as u32)
            .collect();

        // delete_pages expects &[u32] of 1-based page numbers
        clone.delete_pages(&pages_to_delete);
        clone.prune_objects();

        results.push(clone);
    }

    Ok(results)
}

/// Deep-copy a document via serialize round-trip.
///
/// `save_to` normalises the document (updates xref tables, resolves max_id),
/// producing a guaranteed-valid independent PDF. Takes `&mut Document` because
/// `save_to` modifies internal xref tables.
pub(crate) fn duplicate_document(doc: &mut Document) -> Result<Document, String> {
    let mut buf = Cursor::new(Vec::new());
    doc.save_to(&mut buf)
        .map_err(|e| format!("Failed to serialize document for duplication: {}", e))?;
    Document::load_mem(&buf.into_inner())
        .map_err(|e| format!("Failed to reload duplicated document: {}", e))
}

/// Merge multiple documents into a single document.
///
/// Follows lopdf's merge.rs example: renumber objects from each input to avoid
/// ID collisions, collect all pages into a single /Pages tree, build a new
/// catalog. Skips bookmark handling (PatentSafe doesn't use PDF bookmarks).
pub(crate) fn merge_documents(docs: &[&Document]) -> Result<Document, String> {
    if docs.is_empty() {
        return Err("Cannot merge an empty list of documents".to_string());
    }

    let mut merged = Document::with_version("1.5");
    let mut max_id: u32 = 1;
    let mut all_page_objects: Vec<(ObjectId, Object)> = Vec::new();
    let mut all_other_objects: Vec<(ObjectId, Object)> = Vec::new();

    for input in docs {
        let mut clone = (*input).clone();

        // Renumber all objects to avoid collisions with previously merged docs
        clone.renumber_objects_with(max_id);
        max_id = clone.max_id + 1;

        // Separate pages from other objects
        let pages: BTreeMap<u32, ObjectId> = clone.get_pages();
        let page_ids: HashSet<ObjectId> = pages.values().copied().collect();

        for (id, object) in clone.objects {
            if page_ids.contains(&id) {
                // Page object — collect separately for the new page tree
                all_page_objects.push((id, object));
            } else if let Object::Dictionary(ref dict) = object {
                // Skip Catalog and Pages nodes by /Type — we build new ones.
                // This is more robust than looking up the Pages root by ID,
                // and correctly handles multi-level page trees by flattening
                // all intermediate /Pages nodes.
                let obj_type = dict.get(b"Type").ok();
                if obj_type == Some(&Object::Name(b"Catalog".to_vec()))
                    || obj_type == Some(&Object::Name(b"Pages".to_vec()))
                {
                    continue;
                }
                all_other_objects.push((id, object));
            } else {
                all_other_objects.push((id, object));
            }
        }
    }

    // Insert all non-page, non-catalog objects
    for (id, object) in all_other_objects {
        merged.objects.insert(id, object);
    }

    // Insert page objects
    let mut kids: Vec<Object> = Vec::new();
    for (page_id, page_obj) in all_page_objects {
        kids.push(Object::Reference(page_id));
        merged.objects.insert(page_id, page_obj);
    }

    // Update max_id AFTER inserting all objects (other + pages) so that
    // new_object_id() won't collide. Page objects can have higher IDs
    // than non-page objects from the same renumbered clone.
    merged.max_id = merged
        .objects
        .keys()
        .map(|&(id, _)| id)
        .max()
        .unwrap_or(0);

    // Build the new /Pages tree
    let pages_root_id = merged.new_object_id();

    // Update each page's /Parent to point to the new Pages root
    for kid in &kids {
        if let Object::Reference(page_id) = kid {
            if let Ok(Object::Dictionary(ref mut page_dict)) = merged.get_object_mut(*page_id) {
                page_dict.set("Parent", Object::Reference(pages_root_id));
            }
        }
    }

    let mut pages_dict = Dictionary::new();
    pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
    pages_dict.set("Kids", Object::Array(kids.clone()));
    pages_dict.set("Count", Object::Integer(kids.len() as i64));
    merged
        .objects
        .insert(pages_root_id, Object::Dictionary(pages_dict));

    // Build the new catalog
    let mut catalog = Dictionary::new();
    catalog.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog.set("Pages", Object::Reference(pages_root_id));
    let catalog_id = merged.add_object(catalog);

    // Set the trailer root
    merged
        .trailer
        .set("Root", Object::Reference(catalog_id));

    // Clean up numbering
    merged.renumber_objects();
    merged.max_id = merged.objects.keys().map(|&(id, _)| id).max().unwrap_or(0);

    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::Stream;

    /// Build a test PDF with the given number of pages.
    /// Each page has a US Letter MediaBox and an empty content stream.
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
            page_dict.set(
                "MediaBox",
                Object::Array(vec![
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(612),
                    Object::Integer(792),
                ]),
            );
            page_dict.set("Contents", Object::Reference(content_id));
            let page_id = doc.add_object(page_dict);
            kids.push(Object::Reference(page_id));
        }

        let mut pages_dict = Dictionary::new();
        pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
        pages_dict.set("Kids", Object::Array(kids));
        pages_dict.set("Count", Object::Integer(page_count as i64));
        doc.objects
            .insert(pages_id, Object::Dictionary(pages_dict));

        let mut catalog = Dictionary::new();
        catalog.set("Type", Object::Name(b"Catalog".to_vec()));
        catalog.set("Pages", Object::Reference(pages_id));
        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", Object::Reference(catalog_id));

        doc
    }

    // ── Rotation tests ──────────────────────────────────────────────────

    #[test]
    fn test_rotate_90() {
        let mut doc = create_test_pdf(2);
        rotate_all_pages(&mut doc, 90).unwrap();

        for (_page_num, page_id) in doc.get_pages() {
            if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
                assert_eq!(page_dict.get(b"Rotate").unwrap(), &Object::Integer(90));
            } else {
                panic!("Page object is not a dictionary");
            }
        }
    }

    #[test]
    fn test_rotate_cumulative() {
        let mut doc = create_test_pdf(1);

        // Set existing rotation to 90
        let page_id = *doc.get_pages().values().next().unwrap();
        if let Ok(Object::Dictionary(ref mut page_dict)) = doc.get_object_mut(page_id) {
            page_dict.set("Rotate", Object::Integer(90));
        }

        rotate_all_pages(&mut doc, 180).unwrap();

        if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
            assert_eq!(page_dict.get(b"Rotate").unwrap(), &Object::Integer(270));
        } else {
            panic!("Page object is not a dictionary");
        }
    }

    #[test]
    fn test_rotate_360_wraps() {
        let mut doc = create_test_pdf(1);
        rotate_all_pages(&mut doc, 270).unwrap();
        rotate_all_pages(&mut doc, 90).unwrap();

        let page_id = *doc.get_pages().values().next().unwrap();
        if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
            assert_eq!(page_dict.get(b"Rotate").unwrap(), &Object::Integer(0));
        } else {
            panic!("Page object is not a dictionary");
        }
    }

    #[test]
    fn test_rotate_invalid_angle() {
        let mut doc = create_test_pdf(1);
        let result = rotate_all_pages(&mut doc, 45);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid rotation angle 45"));
    }

    #[test]
    fn test_rotate_zero() {
        let mut doc = create_test_pdf(1);
        rotate_all_pages(&mut doc, 0).unwrap();

        let page_id = *doc.get_pages().values().next().unwrap();
        if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
            assert_eq!(page_dict.get(b"Rotate").unwrap(), &Object::Integer(0));
        } else {
            panic!("Page object is not a dictionary");
        }
    }

    // ── Split tests ─────────────────────────────────────────────────────

    #[test]
    fn test_split_3_pages() {
        let doc = create_test_pdf(3);
        let pages = split_pages(&doc).unwrap();
        assert_eq!(pages.len(), 3);
        for page_doc in &pages {
            assert_eq!(page_doc.get_pages().len(), 1);
        }
    }

    #[test]
    fn test_split_single_page() {
        let doc = create_test_pdf(1);
        let pages = split_pages(&doc).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].get_pages().len(), 1);
    }

    #[test]
    fn test_split_preserves_content() {
        let doc = create_test_pdf(2);
        let pages = split_pages(&doc).unwrap();

        for page_doc in &pages {
            let page_id = *page_doc.get_pages().values().next().unwrap();
            if let Ok(Object::Dictionary(ref page_dict)) = page_doc.get_object(page_id) {
                // Each page should have a /Contents reference
                assert!(
                    page_dict.get(b"Contents").is_ok(),
                    "Split page should have /Contents"
                );
            } else {
                panic!("Page object is not a dictionary");
            }
        }
    }

    #[test]
    fn test_split_page_dimensions() {
        let doc = create_test_pdf(3);
        let pages = split_pages(&doc).unwrap();

        for page_doc in &pages {
            let page_id = *page_doc.get_pages().values().next().unwrap();
            if let Ok(Object::Dictionary(ref page_dict)) = page_doc.get_object(page_id) {
                let media_box = page_dict.get(b"MediaBox").unwrap();
                if let Object::Array(ref arr) = media_box {
                    assert_eq!(arr.len(), 4);
                    assert_eq!(arr[2], Object::Integer(612));
                    assert_eq!(arr[3], Object::Integer(792));
                } else {
                    panic!("MediaBox is not an array");
                }
            } else {
                panic!("Page object is not a dictionary");
            }
        }
    }

    // ── Merge tests ─────────────────────────────────────────────────────

    #[test]
    fn test_merge_two_docs() {
        let doc1 = create_test_pdf(2);
        let doc2 = create_test_pdf(3);
        let merged = merge_documents(&[&doc1, &doc2]).unwrap();
        assert_eq!(merged.get_pages().len(), 5);
    }

    #[test]
    fn test_merge_single_doc() {
        let doc = create_test_pdf(3);
        let merged = merge_documents(&[&doc]).unwrap();
        assert_eq!(merged.get_pages().len(), 3);
    }

    #[test]
    fn test_merge_empty() {
        let result = merge_documents(&[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty list"));
    }

    #[test]
    fn test_merge_preserves_page_order() {
        // Create two docs with different page sizes to distinguish them
        let mut doc1 = Document::new();
        {
            let pages_id = doc1.new_object_id();
            let content_id = doc1.add_object(Stream::new(Dictionary::new(), Vec::new()));
            let mut page_dict = Dictionary::new();
            page_dict.set("Type", Object::Name(b"Page".to_vec()));
            page_dict.set("Parent", Object::Reference(pages_id));
            page_dict.set(
                "MediaBox",
                Object::Array(vec![
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(100),
                    Object::Integer(200),
                ]),
            );
            page_dict.set("Contents", Object::Reference(content_id));
            let page_id = doc1.add_object(page_dict);

            let mut pages_dict = Dictionary::new();
            pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
            pages_dict.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
            pages_dict.set("Count", Object::Integer(1));
            doc1.objects
                .insert(pages_id, Object::Dictionary(pages_dict));

            let mut catalog = Dictionary::new();
            catalog.set("Type", Object::Name(b"Catalog".to_vec()));
            catalog.set("Pages", Object::Reference(pages_id));
            let catalog_id = doc1.add_object(catalog);
            doc1.trailer.set("Root", Object::Reference(catalog_id));
        }

        let mut doc2 = Document::new();
        {
            let pages_id = doc2.new_object_id();
            let content_id = doc2.add_object(Stream::new(Dictionary::new(), Vec::new()));
            let mut page_dict = Dictionary::new();
            page_dict.set("Type", Object::Name(b"Page".to_vec()));
            page_dict.set("Parent", Object::Reference(pages_id));
            page_dict.set(
                "MediaBox",
                Object::Array(vec![
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(300),
                    Object::Integer(400),
                ]),
            );
            page_dict.set("Contents", Object::Reference(content_id));
            let page_id = doc2.add_object(page_dict);

            let mut pages_dict = Dictionary::new();
            pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
            pages_dict.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
            pages_dict.set("Count", Object::Integer(1));
            doc2.objects
                .insert(pages_id, Object::Dictionary(pages_dict));

            let mut catalog = Dictionary::new();
            catalog.set("Type", Object::Name(b"Catalog".to_vec()));
            catalog.set("Pages", Object::Reference(pages_id));
            let catalog_id = doc2.add_object(catalog);
            doc2.trailer.set("Root", Object::Reference(catalog_id));
        }

        let merged = merge_documents(&[&doc1, &doc2]).unwrap();
        let pages = merged.get_pages();
        assert_eq!(pages.len(), 2);

        // First page should be 100x200 (from doc1)
        let first_page_id = pages[&1];
        if let Ok(Object::Dictionary(ref page_dict)) = merged.get_object(first_page_id) {
            if let Ok(Object::Array(ref mb)) = page_dict.get(b"MediaBox") {
                assert_eq!(mb[2], Object::Integer(100));
                assert_eq!(mb[3], Object::Integer(200));
            } else {
                panic!("First page has no MediaBox");
            }
        }

        // Second page should be 300x400 (from doc2)
        let second_page_id = pages[&2];
        if let Ok(Object::Dictionary(ref page_dict)) = merged.get_object(second_page_id) {
            if let Ok(Object::Array(ref mb)) = page_dict.get(b"MediaBox") {
                assert_eq!(mb[2], Object::Integer(300));
                assert_eq!(mb[3], Object::Integer(400));
            } else {
                panic!("Second page has no MediaBox");
            }
        }
    }

    #[test]
    fn test_merge_many_pages() {
        // Hot-path coverage for the page_ids membership check (AMPHTT-1161):
        // three inputs with double-digit page counts run the renumber-then-
        // classify cycle multiple times with different max_id offsets. A page
        // the set misses never joins /Kids and drops the count; a non-page it
        // wrongly claims leaves a /Kids entry without a real content stream.
        let doc1 = create_test_pdf(12);
        let doc2 = create_test_pdf(8);
        let doc3 = create_test_pdf(5);
        let merged = merge_documents(&[&doc1, &doc2, &doc3]).unwrap();
        assert_eq!(merged.get_pages().len(), 25);

        for (_page_num, page_id) in merged.get_pages() {
            let stream_ids = merged.get_page_contents(page_id);
            assert!(
                !stream_ids.is_empty(),
                "merged page {page_id:?} has no content stream — page/non-page misclassification"
            );
            for stream_id in stream_ids {
                assert!(
                    matches!(merged.get_object(stream_id), Ok(Object::Stream(_))),
                    "merged page {page_id:?} /Contents -> {stream_id:?} is not a stream"
                );
            }
        }
    }

    // ── Duplicate tests ─────────────────────────────────────────────────

    #[test]
    fn test_duplicate_page_count() {
        let mut doc = create_test_pdf(3);
        let dup = duplicate_document(&mut doc).unwrap();
        assert_eq!(dup.get_pages().len(), 3);
    }

    #[test]
    fn test_duplicate_independence() {
        let mut doc = create_test_pdf(1);
        let dup = duplicate_document(&mut doc).unwrap();

        // Modify original — add a rotation
        let page_id = *doc.get_pages().values().next().unwrap();
        if let Ok(Object::Dictionary(ref mut page_dict)) = doc.get_object_mut(page_id) {
            page_dict.set("Rotate", Object::Integer(90));
        }

        // Duplicate should be unaffected
        let dup_page_id = *dup.get_pages().values().next().unwrap();
        if let Ok(Object::Dictionary(ref page_dict)) = dup.get_object(dup_page_id) {
            assert!(
                page_dict.get(b"Rotate").is_err(),
                "Duplicate should not have /Rotate"
            );
        }
    }

    #[test]
    fn test_merge_split_round_trip() {
        // Split a synthetic multi-page PDF then merge it back. The source is
        // built in-process via `create_test_pdf` (no on-disk fixture) so the
        // test runs anywhere, including the gem's CI.
        //
        // This guards a real object-ID collision bug in `merge_documents`. The
        // buggy version never recomputed `merged.max_id` after inserting the
        // renumbered objects, so it stayed at 0 and `new_object_id()` (plus the
        // `add_object` for the catalog) returned ids that were already in use,
        // silently overwriting an existing object. On the original fixture that
        // clobbered object was a *page*, so page count dropped. With a synthetic
        // doc the clobbered object is a content stream, so the page count is
        // preserved but a page is left referencing a non-stream object — hence
        // the content-integrity check below, which catches the collision even
        // when `get_pages().len()` does not. Remove the `merged.max_id`
        // recompute in `merge_documents` and that check fails.
        let doc = create_test_pdf(3);
        let page_count = doc.get_pages().len();
        assert!(page_count >= 2, "Test needs a multi-page PDF");

        let splits = split_pages(&doc).unwrap();
        assert_eq!(splits.len(), page_count);

        let refs: Vec<&Document> = splits.iter().collect();
        let merged = merge_documents(&refs).unwrap();
        assert_eq!(merged.get_pages().len(), page_count);

        // Every merged page must still reference a real content stream. An
        // object-ID collision overwrites a referenced object, leaving a page
        // pointing at a non-stream (or nothing) — this is the invariant the
        // original round-trip test existed to protect.
        for (_page_num, page_id) in merged.get_pages() {
            let stream_ids = merged.get_page_contents(page_id);
            assert!(
                !stream_ids.is_empty(),
                "merged page {page_id:?} has no content stream — object-ID collision clobbered it"
            );
            for stream_id in stream_ids {
                assert!(
                    matches!(merged.get_object(stream_id), Ok(Object::Stream(_))),
                    "merged page {page_id:?} /Contents -> {stream_id:?} is not a stream — object-ID collision"
                );
            }
        }
    }

    #[test]
    fn test_duplicate_round_trip() {
        let mut doc = create_test_pdf(2);
        let mut dup = duplicate_document(&mut doc).unwrap();

        // The duplicate should be saveable (valid PDF structure)
        let mut buf = Cursor::new(Vec::new());
        dup.save_to(&mut buf).expect("Duplicate should be saveable");
        let bytes = buf.into_inner();
        assert!(!bytes.is_empty());

        // Reload should work
        let reloaded = Document::load_mem(&bytes).expect("Reloaded duplicate should be valid");
        assert_eq!(reloaded.get_pages().len(), 2);
    }
}
