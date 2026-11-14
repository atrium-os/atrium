//! Image decoding for the Navigator's jailed workers (threat model §2.4):
//! the document worker and the page view decode a page's images in the jail
//! that already holds its hostile input, and hand on only a checked RGBA8
//! buffer. Split out of navigator-backend so a worker that decodes images
//! links the decoders and nothing else of the broker's (no fetcher, no TLS).

pub mod image;
pub mod resample;
pub mod svg;
