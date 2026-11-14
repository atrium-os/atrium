//! Officina's converters (docs/spec/officina.md §5.2).
//!
//! Foreign files are parsed in a converter process, never in the document's: the
//! `officina-convert` binary reads the foreign bytes on stdin, sandboxes itself (on
//! FreeBSD, Capsicum capability mode before it parses a byte: no files, no sockets),
//! and writes a native file on stdout and a fidelity report on stderr. Writers run in
//! the document process, over our own validated model.

pub mod docx;
pub mod odt;
pub mod ods;
pub mod report;
pub mod xlsx;
pub mod xml;
pub mod zip;
