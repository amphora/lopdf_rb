//! Object-id → dictionary resolution helpers shared by the write-path
//! modules. Introduced in stamp.rs (AMPHTT-1159) and promoted here when
//! manipulation.rs became the second consumer (AMPHTT-1233); this is the
//! crate's canonical resolve-or-error idiom for dictionary writes the
//! caller's contract requires to succeed. Contract pinned by call-site
//! tests in stamp.rs (`test_ensure_font_errors_when_page_unresolvable`)
//! and manipulation.rs (the `rotate_page` / `set_page_parent` error-path
//! tests).

use lopdf::{Dictionary, Document, Object, ObjectId};

/// Resolve an object id to its mutable dictionary for a content-stream
/// write, failing with `"<action>: <lopdf error>"`. Covers both failure
/// shapes the old silent `if let Ok(Object::Dictionary(...))` guards
/// swallowed: `get_object_mut` errors and ok-but-not-a-Dictionary objects.
/// The action closure is only evaluated on the error path.
pub(crate) fn require_dict_mut(
    doc: &mut Document,
    id: ObjectId,
    action: impl FnOnce() -> String,
) -> Result<&mut Dictionary, String> {
    doc.get_object_mut(id)
        .and_then(Object::as_dict_mut)
        .map_err(|e| format!("{}: {}", action(), e))
}

/// Immutable sibling of [`require_dict_mut`]: resolve an object id to its
/// dictionary for a read the caller's contract requires to succeed,
/// failing with `"<action>: <lopdf error>"`. The action closure is only
/// evaluated on the error path.
pub(crate) fn require_dict(
    doc: &Document,
    id: ObjectId,
    action: impl FnOnce() -> String,
) -> Result<&Dictionary, String> {
    doc.get_object(id)
        .and_then(Object::as_dict)
        .map_err(|e| format!("{}: {}", action(), e))
}
