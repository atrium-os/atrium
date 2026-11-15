//! The budgets of folium.md §6, in one place. Each has a test that a file
//! just past it is refused cleanly.

/// The largest file read (pages are read lazily, so a big file costs only
/// what is asked of it).
pub const FILE_MAX: u64 = 1 << 30;
/// Object numbers at or above this are ignored.
pub const OBJECTS_MAX: usize = 8 << 20;
/// Nesting of arrays and dictionaries.
pub const DEPTH_MAX: usize = 64;
/// One object outside a stream, in bytes of syntax.
pub const OBJECT_BYTES_MAX: usize = 16 << 20;
/// One stream, encoded.
pub const STREAM_ENCODED_MAX: u64 = 256 << 20;
/// One stream, decoded.
pub const STREAM_DECODED_MAX: usize = 256 << 20;
/// All decoded streams held at once, per document.
pub const DOC_DECODED_MAX: usize = 512 << 20;
/// Filters chained on one stream.
pub const FILTERS_MAX: usize = 8;
/// `/Prev` links followed in the cross-reference chain.
pub const XREF_CHAIN_MAX: usize = 64;
/// References that point at references.
pub const REF_CHAIN_MAX: usize = 32;
/// Objects kept parsed in the document's cache before it is cleared.
pub const CACHE_OBJECTS_MAX: usize = 1 << 16;
/// Decoded object streams kept.
pub const CACHE_OBJSTM_MAX: usize = 16;
/// How far past a stream's stated end `endstream` is looked for, and how
/// far a stream with a wrong `/Length` is searched.
pub const ENDSTREAM_SEARCH_MAX: u64 = 64 << 20;
