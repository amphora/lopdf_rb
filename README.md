# lopdf_rb

Ruby native extension wrapping the Rust [lopdf](https://crates.io/crates/lopdf) crate for PDF document manipulation. Built with [magnus](https://github.com/matsadler/magnus) and [rb-sys](https://github.com/oxidize-rb/rb-sys).

Part of the [PatentSafe](https://www.amphora.net/patentsafe) PDF pipeline — replaces subprocess-based PDF tooling with direct Ruby API calls.

## API

```ruby
require "lopdf_rb"

# Load from file
doc = LopdfRb::Document.load("/path/to/file.pdf")

# Load from binary string
doc = LopdfRb::Document.from_bytes(pdf_bytes)

# Inspect
doc.page_count                # => 3
doc.page_dimensions(0)        # => { width: 612.0, height: 792.0 }

# Save to file
doc.save("/path/to/output.pdf")

# Serialize to binary string
bytes = doc.to_bytes
```

### `LopdfRb::Document.load(path)` → `Document`

Load a PDF from a file path. Raises `RuntimeError` if the file cannot be read or parsed.

### `LopdfRb::Document.from_bytes(string)` → `Document`

Load a PDF from a binary string (`String` with `ASCII-8BIT` encoding). Raises `RuntimeError` if the data is not valid PDF.

### `#page_count` → `Integer`

Returns the number of pages in the document.

### `#page_dimensions(page_index)` → `Hash`

Returns a frozen hash `{ width: Float, height: Float }` with the page dimensions in PDF points (1 point = 1/72 inch).

`page_index` is **0-based**. Raises `ArgumentError` if the index is out of range.

Resolves inherited `/MediaBox` and `/CropBox` attributes by walking the page tree per ISO 32000 §7.7.3.4. Falls back to US Letter (612 × 792 points) if no valid bounding box is found, and prints a warning to stderr.

### `#stamp_metadata(reader, ip, timestamp, unique_id)` → `nil`

Write custom fields to the PDF's `/Info` dictionary:

- `Reader` — the reader's display name
- `ReaderIP` — the reader's IP address
- `ReadTimestamp` — ISO 8601 timestamp of the read event
- `UniqueID` — UUID for this specific read event

Creates the `/Info` dictionary if it doesn't exist. Preserves existing entries — with one exception: if a malformed PDF stores `/Info` as a direct dictionary in the trailer (ISO 32000 requires an indirect reference), it is replaced with a fresh dictionary and its entries are **not** preserved.

ASCII values are stored verbatim; non-ASCII values (e.g. a reader name like "José") are stored as UTF-16BE with a BOM prefix in a hexadecimal string, per ISO 32000 §7.9.2 (via lopdf's `text_string` encoder).

Raises `ArgumentError` if any argument exceeds 512 bytes (UTF-8). Raises `RuntimeError` if the `/Info` entry cannot be resolved to a dictionary (a dangling reference, or a reference to a non-dictionary object).

```ruby
doc.stamp_metadata(
  "Alex Researcher",
  "10.0.0.1",
  Time.now.utc.iso8601(3),
  SecureRandom.uuid
)
```

### `#add_dlp_annotation(dlp_data)` → `nil`

Add a hidden FreeText annotation to the **last page** of the document. The annotation is invisible (Hidden + NoView flags, F=34) and contains the provided string as its `/Contents`.

Typically used to embed a JSON blob with reader/document metadata for DLP (Data Loss Prevention) purposes.

```ruby
doc.add_dlp_annotation('{"reader":"Alex","documentId":"DOC-123","timestamp":"2026-03-16T11:00:00.000Z"}')
```

Raises `RuntimeError` if the PDF has no pages, if the last page's dictionary cannot be accessed, or if the page's `/Annots` is an indirect reference that does not resolve to an array (overwriting it would silently discard the page's existing annotations, so the operation fails instead). On failure the pending annotation object is removed again, leaving the document as it was before the call.

### `#apply_visible_stamps(config)` → `nil`

Render visible stamps, text blocks, lines, and rectangles on **every page** of the document.

Takes a Hash (converted to JSON internally) with four optional arrays:

```ruby
doc.apply_visible_stamps({
  stamps: [{
    text: "viewed by Alex",
    x: 4, y: 8,
    origin_x: "left",     # "left" | "right" | "centre"
    origin_y: "top",      # "top" | "bottom" | "middle"
    size: 8,
    color: [0.5, 0.5, 0.5],  # RGB 0.0-1.0
    font: "Helvetica",        # optional, defaults to Helvetica
    align: "left",            # optional: "left" | "centre" | "right"
    vertical_align: "bottom", # optional: "bottom" | "top" | "middle"
    rotation: nil             # optional: degrees counter-clockwise
  }],
  text_blocks: [{
    text: "Long text that wraps...",
    x: 10, y: 700,
    width: 200,           # max width in points before wrapping
    line_spacing: 14,     # vertical spacing between lines
    size: 12,             # optional, defaults to 12
    color: [0, 0, 0]      # optional, defaults to black
  }],
  lines: [{
    x1: 0, y1: 780, x2: 612, y2: 780,
    color: [0, 0, 0],     # optional, defaults to black
    thickness: 0.5        # optional, defaults to 0.5
  }],
  rectangles: [{
    x1: 5, y1: 5, x2: 607, y2: 787,
    color: [0.8, 0.8, 0.8], # optional
    thickness: 1.0           # optional
  }]
})
```

Raises `ArgumentError` if the config hash cannot be deserialized, and `RuntimeError` if a stamp cannot be applied (e.g. a font cannot be registered into a page's `/Resources`) — discard the document on error rather than saving it.

`/Font` inside a page's `/Resources` is handled as either a direct dictionary or an indirect reference (both legal per ISO 32000 §7.8.3), and entries inside it are found whether stored as references or direct dictionaries; a `/Font` reference that does not resolve to a dictionary raises rather than silently leaving the stamp text unrendered. A `/Resources` entry that is an unresolvable indirect reference — on the page itself or on a `/Parent` ancestor consulted for inheritance — also raises, rather than being silently replaced or skipped over. Fonts registered on a page with inherited (parent-level) resources never mutate the shared parent dictionaries — the page gets its own resources copy. Resource names referenced in generated content streams are `#XX`-encoded per ISO 32000 §7.3.5, so an existing font entry whose key contains delimiters, `#`, or non-ASCII bytes is reused and referenced byte-faithfully rather than emitted corrupted.

Colour channels outside 0.0–1.0 are clamped into range (DeviceRGB operands must be in [0.0, 1.0] per the PDF spec).

### `#save(path)` → `nil`

Save the document to a file. Raises `RuntimeError` on I/O failure.

### `#to_bytes` → `String`

Serialize the document to a binary string (`ASCII-8BIT` encoding).

## Building

Requires Rust (stable) and the `rb_sys` gem:

```bash
cd gems/lopdf_rb
cargo build --release    # compile the native extension
cargo test               # run Rust unit tests
cargo clippy             # lint
```

CI (`.github/workflows/ci.yml`) runs `cargo test --workspace` on every
pull request and push to `main`.

## License

MIT — see [LICENSE](LICENSE).
