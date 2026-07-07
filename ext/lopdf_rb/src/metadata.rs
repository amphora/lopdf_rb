use lopdf::{Document, Object, Dictionary, StringFormat};

/// Maximum accepted byte length (UTF-8, as received from Ruby) for each
/// user-supplied /Info field. An abuse guard against Info-dict bloat, not a
/// functional limit — real values are tens of bytes.
pub(crate) const MAX_FIELD_BYTES: usize = 255;

/// Validate `(name, value)` pairs against [`MAX_FIELD_BYTES`], naming the
/// offending field in the error. Lengths are measured on the UTF-8 input,
/// before any text-string encoding.
pub(crate) fn validate_field_lengths(fields: &[(&str, &str)]) -> Result<(), String> {
    for (name, value) in fields {
        if value.len() > MAX_FIELD_BYTES {
            return Err(format!(
                "{} exceeds the maximum length of {} bytes (got {} bytes)",
                name,
                MAX_FIELD_BYTES,
                value.len()
            ));
        }
    }
    Ok(())
}

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
            dict.set("Reader", Object::String(encode_text_string(reader), StringFormat::Literal));
            dict.set("ReadTimestamp", Object::String(encode_text_string(timestamp), StringFormat::Literal));
            dict.set("UniqueID", Object::String(encode_text_string(unique_id), StringFormat::Literal));
            dict.set("ReaderIP", Object::String(encode_text_string(ip), StringFormat::Literal));
            Ok(())
        }
        Ok(_) => Err(format!("/Info object {:?} is not a dictionary", info_id)),
        Err(e) => Err(format!("failed to resolve /Info object {:?}: {}", info_id, e)),
    }
}

/// Encode a UTF-8 string for storage as a PDF text string (ISO 32000 §7.9.2):
/// ASCII passes through verbatim (ASCII is a subset of PDFDocEncoding);
/// anything else becomes UTF-16BE prefixed with the \xFE\xFF BOM. Raw UTF-8
/// bytes are not a valid text-string encoding and viewers mis-decode them.
///
/// The UTF-16BE bytes may contain 0x0D (e.g. "č" = U+010D), which an
/// unescaped Literal string would corrupt to 0x0A on read; safe here because
/// lopdf's writer (pinned =0.34) escapes `\r` in Literal strings.
fn encode_text_string(s: &str) -> Vec<u8> {
    if s.is_ascii() {
        return s.as_bytes().to_vec();
    }
    let mut bytes = Vec::with_capacity(2 + s.len() * 2);
    bytes.extend_from_slice(&[0xFE, 0xFF]);
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    bytes
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
    fn test_encode_text_string_ascii_passthrough() {
        assert_eq!(encode_text_string("Alex"), b"Alex".to_vec());
    }

    #[test]
    fn test_encode_text_string_non_ascii_utf16be() {
        assert_eq!(
            encode_text_string("José"),
            vec![0xFE, 0xFF, 0x00, 0x4A, 0x00, 0x6F, 0x00, 0x73, 0x00, 0xE9]
        );
        // "č" (U+010D): its UTF-16BE bytes contain 0x0D — the CR byte the
        // Literal-string carrier depends on lopdf's writer escaping.
        assert_eq!(encode_text_string("č"), vec![0xFE, 0xFF, 0x01, 0x0D]);
    }

    #[test]
    fn test_set_metadata_encodes_non_ascii_reader_as_utf16be() {
        let mut doc = create_empty_pdf();

        set_metadata(&mut doc, "José", "2026-01-01T00:00:00.000Z", "uuid-123", "10.0.0.1").unwrap();

        let info_ref = doc.trailer.get(b"Info").unwrap().as_reference().unwrap();
        if let Ok(Object::Dictionary(ref dict)) = doc.get_object(info_ref) {
            assert_eq!(
                dict.get(b"Reader").unwrap(),
                &Object::String(
                    vec![0xFE, 0xFF, 0x00, 0x4A, 0x00, 0x6F, 0x00, 0x73, 0x00, 0xE9],
                    StringFormat::Literal
                )
            );
            // ASCII siblings in the same call stay verbatim
            assert_eq!(
                dict.get(b"ReaderIP").unwrap(),
                &Object::String(b"10.0.0.1".to_vec(), StringFormat::Literal)
            );
        } else {
            panic!("Info object should be a dictionary");
        }
    }

    #[test]
    fn test_validate_field_lengths_accepts_cap() {
        let at_cap = "a".repeat(MAX_FIELD_BYTES);
        assert!(validate_field_lengths(&[("reader", &at_cap)]).is_ok());
    }

    #[test]
    fn test_validate_field_lengths_rejects_over_cap() {
        let over_cap = "a".repeat(MAX_FIELD_BYTES + 1);
        let err = validate_field_lengths(&[("reader", "ok"), ("unique_id", &over_cap)]).unwrap_err();
        assert!(err.starts_with("unique_id exceeds"), "unexpected error: {err}");
    }

    #[test]
    fn test_validate_field_lengths_counts_bytes_not_chars() {
        // 128 chars but 256 UTF-8 bytes — the cap is on bytes
        let multibyte = "é".repeat(128);
        assert!(validate_field_lengths(&[("reader", &multibyte)]).is_err());
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
