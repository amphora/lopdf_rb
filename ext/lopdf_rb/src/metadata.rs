use lopdf::{text_string, Document, Object, Dictionary};

/// Maximum accepted byte length (UTF-8, as received from Ruby) for each
/// user-supplied /Info field — an abuse guard, not a functional limit.
/// 512 covers the application's 64-character display-name limit even at
/// 4 UTF-8 bytes per character (256 bytes), with headroom. The cap is
/// measured on the input bytes; the encoded on-disk form (a UTF-16BE
/// hexadecimal string for non-ASCII values) may be larger.
pub(crate) const MAX_FIELD_BYTES: usize = 512;

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

/// Write the four read-event fields to the document's `/Info` dictionary.
///
/// Parameters (same order as the Ruby-facing `RbDocument::stamp_metadata`):
/// - `reader` — the reader's display name (`/Reader`)
/// - `ip` — the reader's IP address (`/ReaderIP`)
/// - `timestamp` — ISO 8601 read timestamp (`/ReadTimestamp`)
/// - `unique_id` — UUID for this read event (`/UniqueID`)
///
/// Values are encoded with [`lopdf::text_string`]: ASCII verbatim as a
/// Literal string, non-ASCII as UTF-16BE with a BOM in a Hexadecimal
/// string (ISO 32000 §7.9.2).
///
/// A missing `/Info`, or one that is not an indirect reference (a direct
/// dictionary is malformed — the trailer's `/Info` must be a reference), is
/// replaced with a fresh indirect dictionary; entries carried by a direct
/// dictionary are dropped. Errors when `/Info` is a dangling reference or
/// resolves to a non-dictionary.
pub(crate) fn set_metadata(doc: &mut Document, reader: &str, ip: &str, timestamp: &str, unique_id: &str) -> Result<(), String> {
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
            for (key, value) in [
                ("Reader", reader),
                ("ReaderIP", ip),
                ("ReadTimestamp", timestamp),
                ("UniqueID", unique_id),
            ] {
                dict.set(key, text_string(value));
            }
            Ok(())
        }
        Ok(_) => Err(format!("/Info object {:?} is not a dictionary", info_id)),
        Err(e) => Err(format!("failed to resolve /Info object {:?}: {}", info_id, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Document, StringFormat};

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

        set_metadata(&mut doc, "Alex", "10.0.0.1", "2026-03-16T11:00:00.000Z", "550e8400-uuid").unwrap();

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

        set_metadata(&mut doc, "Bob", "192.168.1.1", "2026-01-01T00:00:00.000Z", "uuid-123").unwrap();

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
    fn test_set_metadata_encodes_non_ascii_reader_as_utf16be() {
        let mut doc = create_empty_pdf();

        set_metadata(&mut doc, "José", "10.0.0.1", "2026-01-01T00:00:00.000Z", "uuid-123").unwrap();

        let info_ref = doc.trailer.get(b"Info").unwrap().as_reference().unwrap();
        if let Ok(Object::Dictionary(ref dict)) = doc.get_object(info_ref) {
            // Pins the lopdf::text_string contract this crate relies on:
            // non-ASCII → BOM-prefixed UTF-16BE in a Hexadecimal string.
            assert_eq!(
                dict.get(b"Reader").unwrap(),
                &Object::String(
                    vec![0xFE, 0xFF, 0x00, 0x4A, 0x00, 0x6F, 0x00, 0x73, 0x00, 0xE9],
                    StringFormat::Hexadecimal
                )
            );
            // ASCII siblings in the same call stay verbatim Literal strings
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
        // 300 chars but 600 UTF-8 bytes — the cap is on bytes
        let multibyte = "é".repeat(300);
        assert!(validate_field_lengths(&[("reader", &multibyte)]).is_err());
    }

    #[test]
    fn test_validate_field_lengths_accepts_max_length_4_byte_char_name() {
        // The app validates display names at 64 CHARACTERS; the worst case
        // is 64 supplementary-plane code points = 256 UTF-8 bytes, which
        // must fit under the cap (it did not under the original 255).
        let name = "\u{1F600}".repeat(64);
        assert_eq!(name.len(), 256);
        assert!(validate_field_lengths(&[("reader", &name)]).is_ok());
    }

    #[test]
    fn test_set_metadata_errors_on_dangling_info_reference() {
        let mut doc = create_empty_pdf();
        doc.trailer.set("Info", Object::Reference((9999, 0)));

        let result = set_metadata(&mut doc, "Alex", "10.0.0.1", "2026-01-01T00:00:00.000Z", "uuid-123");

        let err = result.unwrap_err();
        assert!(err.contains("failed to resolve /Info object"), "unexpected error: {err}");
    }

    #[test]
    fn test_set_metadata_errors_when_info_is_not_a_dictionary() {
        let mut doc = create_empty_pdf();
        let bogus_id = doc.add_object(Object::Integer(42));
        doc.trailer.set("Info", Object::Reference(bogus_id));

        let result = set_metadata(&mut doc, "Alex", "10.0.0.1", "2026-01-01T00:00:00.000Z", "uuid-123");

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

        set_metadata(&mut doc, "Alex", "10.0.0.1", "2026-01-01T00:00:00.000Z", "uuid-123").unwrap();

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
