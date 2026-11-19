//! Officina's converters (docs/spec/officina.md §5.2).
//!
//! Foreign files are parsed in a converter process, never in the document's: the
//! `officina-convert` binary reads the foreign bytes on stdin, sandboxes itself (on
//! FreeBSD, Capsicum capability mode before it parses a byte: no files, no sockets),
//! and writes a native file on stdout and a fidelity report on stderr. Writers run in
//! the document process, over our own validated model.

// Without the readers, helpers only they use are left unused.
#![cfg_attr(not(feature = "read"), allow(dead_code, unused_imports))]

pub mod cfb;
pub mod crypt;
#[cfg(feature = "read")]
pub mod doc;
#[cfg(feature = "read")]
pub mod ppt;
pub mod docx;
pub mod html_clipboard;
pub mod launch;
pub mod odt;
pub mod pictures;
pub mod odfcrypt;
pub mod odp;
pub mod ods;
pub mod pptx;
pub mod report;
pub mod sealed;
#[cfg(feature = "read")]
pub mod xls;
pub mod xlsx;
pub mod xml;
pub mod zip;
