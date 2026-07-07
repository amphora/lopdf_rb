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

Resolves inherited `/MediaBox` and `/CropBox` attributes by walking the page tree per ISO 32000 §7.7.3.4. Falls back to US Letter (612 × 792 points) if no bounding box is found.

### `#stamp_metadata(reader, ip, timestamp, unique_id)` → `nil`

Write custom fields to the PDF's `/Info` dictionary:

- `Reader` — the reader's display name
- `ReaderIP` — the reader's IP address
- `ReadTimestamp` — ISO 8601 timestamp of the read event
- `UniqueID` — UUID for this specific read event

Creates the `/Info` dictionary if it doesn't exist. Preserves existing entries.

ASCII values are stored verbatim; non-ASCII values (e.g. a reader name like "José") are stored as UTF-16BE with a BOM prefix, per ISO 32000 §7.9.2.

Raises `ArgumentError` if any argument exceeds 255 bytes (UTF-8). Raises `RuntimeError` if the `/Info` entry cannot be resolved to a dictionary (a dangling reference, or a reference to a non-dictionary object).

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

Raises `RuntimeError` if the PDF has no pages.

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

Raises `ArgumentError` if the config hash cannot be deserialized.

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

## License

MIT — see [LICENSE](LICENSE).
