use lopdf::{Document, Object, Dictionary, StringFormat};

pub(crate) fn set_metadata(doc: &mut Document, reader: &str, timestamp: &str, unique_id: &str, ip: &str) -> Result<(), String> {
    // Get the existing Info reference, or create a fresh Info dictionary when
    // /Info is missing or not an indirect reference (a direct dictionary is
    // malformed per ISO 32000 — the trailer's /Info must be a reference).
    let existing_id = doc.trailer.get(b"Info").ok().and_then(|obj| obj.as_reference().ok());
    let info_id = match existing_id {
        Some(id) => id,
        None => {
            let new_id = doc.add_object(Dictionary::new());
            doc.trailer.set("Info", Object::Reference(new_id));
            new_id
        }
    };

    match doc.get_object_mut(info_id) {
        Ok(Object::Dictionary(dict)) => {
            dict.set("Reader", Object::String(reader.as_bytes().to_vec(), StringFormat::Literal));
            dict.set("ReadTimestamp", Object::String(timestamp.as_bytes().to_vec(), StringFormat::Literal));
            dict.set("UniqueID", Object::String(unique_id.as_bytes().to_vec(), StringFormat::Literal));
            dict.set("ReaderIP", Object::String(ip.as_bytes().to_vec(), StringFormat::Literal));
            Ok(())
        }
        Ok(_) => Err(format!("/Info object {:?} is not a dictionary", info_id)),
        Err(e) => Err(format!("failed to resolve /Info object {:?}: {}", info_id, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::Document;

    fn create_empty_pdf() -> Document {
        let mut doc = Document::new();
        let mut catalog = Dictionary::new();
        catalog.set("Type", Object::Name(b"Catalog".to_vec()));
        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    #[test]
    fn test_set_metadata_creates_info_dict() {
        let mut doc = create_empty_pdf();
        assert!(doc.trailer.get(b"Info").is_err());

        set_metadata(&mut doc, "Alex", "2026-03-16T11:00:00.000Z", "550e8400-uuid", "10.0.0.1").unwrap();

        let info_ref = doc.trailer.get(b"Info").unwrap().as_reference().unwrap();
        if let Ok(Object::Dictionary(ref dict)) = doc.get_object(info_ref) {
            assert_eq!(
                dict.get(b"Reader").unwrap(),
                &Object::String(b"Alex".to_vec(), StringFormat::Literal)
            );
            assert_eq!(
                dict.get(b"ReadTimestamp").unwrap(),
                &Object::String(b"2026-03-16T11:00:00.000Z".to_vec(), StringFormat::Literal)
            );
            assert_eq!(
                dict.get(b"UniqueID").unwrap(),
                &Object::String(b"550e8400-uuid".to_vec(), StringFormat::Literal)
            );
            assert_eq!(
                dict.get(b"ReaderIP").unwrap(),
                &Object::String(b"10.0.0.1".to_vec(), StringFormat::Literal)
            );
        } else {
            panic!("Info object should be a dictionary");
        }
    }

    #[test]
    fn test_set_metadata_updates_existing_info() {
        let mut doc = create_empty_pdf();
        let mut info = Dictionary::new();
        info.set("Author", Object::String(b"Existing Author".to_vec(), StringFormat::Literal));
        let info_id = doc.add_object(info);
        doc.trailer.set("Info", Object::Reference(info_id));

        set_metadata(&mut doc, "Bob", "2026-01-01T00:00:00.000Z", "uuid-123", "192.168.1.1").unwrap();

        if let Ok(Object::Dictionary(ref dict)) = doc.get_object(info_id) {
            // Original field preserved
            assert_eq!(
                dict.get(b"Author").unwrap(),
                &Object::String(b"Existing Author".to_vec(), StringFormat::Literal)
            );
            // New fields added
            assert_eq!(
                dict.get(b"Reader").unwrap(),
                &Object::String(b"Bob".to_vec(), StringFormat::Literal)
            );
        } else {
            panic!("Info object should be a dictionary");
        }
    }

    #[test]
    fn test_set_metadata_errors_on_dangling_info_reference() {
        let mut doc = create_empty_pdf();
        doc.trailer.set("Info", Object::Reference((9999, 0)));

        let result = set_metadata(&mut doc, "Alex", "2026-01-01T00:00:00.000Z", "uuid-123", "10.0.0.1");

        let err = result.unwrap_err();
        assert!(err.contains("failed to resolve /Info object"), "unexpected error: {err}");
    }

    #[test]
    fn test_set_metadata_errors_when_info_is_not_a_dictionary() {
        let mut doc = create_empty_pdf();
        let bogus_id = doc.add_object(Object::Integer(42));
        doc.trailer.set("Info", Object::Reference(bogus_id));

        let result = set_metadata(&mut doc, "Alex", "2026-01-01T00:00:00.000Z", "uuid-123", "10.0.0.1");

        let err = result.unwrap_err();
        assert!(err.contains("is not a dictionary"), "unexpected error: {err}");
    }

    /// A direct-Dictionary /Info (malformed per ISO 32000 — the trailer's
    /// /Info must be an indirect reference) is NOT a failure path: it fails
    /// `as_reference()` and takes the create-new-dict branch, replacing the
    /// direct dictionary with a fresh indirect one. Entries carried by the
    /// direct dictionary are dropped. Pinned deliberately — see the plan's
    /// Design Decisions 3 and 7 (AMPHTT-1156).
    #[test]
    fn test_set_metadata_replaces_direct_info_dictionary() {
        let mut doc = create_empty_pdf();
        let mut info = Dictionary::new();
        info.set("Author", Object::String(b"Existing Author".to_vec(), StringFormat::Literal));
        doc.trailer.set("Info", Object::Dictionary(info));

        set_metadata(&mut doc, "Alex", "2026-01-01T00:00:00.000Z", "uuid-123", "10.0.0.1").unwrap();

        // The trailer now holds an indirect reference to a fresh dictionary
        // carrying the four fields; the direct dict's entries are gone.
        let info_ref = doc.trailer.get(b"Info").unwrap().as_reference().unwrap();
        if let Ok(Object::Dictionary(ref dict)) = doc.get_object(info_ref) {
            assert_eq!(
                dict.get(b"Reader").unwrap(),
                &Object::String(b"Alex".to_vec(), StringFormat::Literal)
            );
            assert!(dict.get(b"Author").is_err(), "direct dict's entries are not migrated");
        } else {
            panic!("Info object should be a dictionary");
        }
    }
}
