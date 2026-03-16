use lopdf::{Document, Object, Dictionary, StringFormat};

pub(crate) fn set_metadata(doc: &mut Document, reader: &str, timestamp: &str, unique_id: &str, ip: &str) {
    // Get or create the Info dictionary
    let info_id = if let Ok(info_ref) = doc.trailer.get(b"Info") {
        if let Ok(reference) = info_ref.as_reference() {
            reference
        } else {
            // Create a new Info dictionary
            let new_id = doc.add_object(Dictionary::new());
            doc.trailer.set("Info", Object::Reference(new_id));
            new_id
        }
    } else {
        let new_id = doc.add_object(Dictionary::new());
        doc.trailer.set("Info", Object::Reference(new_id));
        new_id
    };

    if let Ok(Object::Dictionary(ref mut dict)) = doc.get_object_mut(info_id) {
        dict.set("Reader", Object::String(reader.as_bytes().to_vec(), StringFormat::Literal));
        dict.set("ReadTimestamp", Object::String(timestamp.as_bytes().to_vec(), StringFormat::Literal));
        dict.set("UniqueID", Object::String(unique_id.as_bytes().to_vec(), StringFormat::Literal));
        dict.set("ReaderIP", Object::String(ip.as_bytes().to_vec(), StringFormat::Literal));
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

        set_metadata(&mut doc, "Alex", "2026-03-16T11:00:00.000Z", "550e8400-uuid", "10.0.0.1");

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

        set_metadata(&mut doc, "Bob", "2026-01-01T00:00:00.000Z", "uuid-123", "192.168.1.1");

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
}
