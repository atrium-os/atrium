//! An app's pictures (dev-tools.md §3.5, app-composition.md C6).
//!
//! The pictures and icons in an app's bundle (`assets/`) are decoded by the
//! system's image decoder, `org.atrium.image-decode`: navigator-image's
//! decoders (PNG, JPEG, GIF, WebP, strict-profile SVG) in a jail of their
//! own, as the system's uid, with no capability. The core hands it an open
//! file and gets RGBA pixels back, so it never parses image bytes and needs
//! no worker of its own for its own pictures. The manifest names it:
//!
//! ```toml
//! [capabilities]
//! decoders = ["image"]
//! ```
//!
//! ```ignore
//! let icon = atrium_assets::image("app.svg", 64, 64)?;   // drawn at 64×64
//! let photo = atrium_assets::image("welcome.jpg", 800, 600)?;
//! ```
//!
//! What comes back is checked here as well: its size within the box asked
//! for, and exactly width × height × 4 bytes.

use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use atrium_message::Message;
pub use atrium_worker::Error as WorkerError;

/// The largest box a picture is decoded into, per side.
pub const MAX_SIDE: u32 = 4096;

/// What the decoder is asked: the box to fit the picture in, one scale for
/// both sides. A raster picture is never made larger than it is; an SVG is
/// drawn at the largest size of its shape that fits.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
#[message(version = 1)]
pub struct Decode {
    #[message(min = 1, max = 4096)]
    pub max_width: u32,
    #[message(min = 1, max = 4096)]
    pub max_height: u32,
}

/// A decoded picture: RGBA8, rows top to bottom, not premultiplied.
#[derive(Message, Clone, PartialEq, Eq)]
#[message(version = 1)]
pub struct Picture {
    #[message(min = 1, max = 4096)]
    pub width: u32,
    #[message(min = 1, max = 4096)]
    pub height: u32,
    #[message(max_len = 67108864)]
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Picture {}x{} ({} bytes)", self.width, self.height, self.rgba.len())
    }
}

/// Why there's no picture.
#[derive(Debug)]
pub enum Error {
    /// The name isn't a file name in `assets/`.
    Name(String),
    /// The file isn't there (or can't be opened).
    Open(std::io::Error),
    /// The box is empty or larger than [`MAX_SIDE`].
    Size(u32, u32),
    /// The decoder refused it or couldn't be reached.
    Decoder(WorkerError),
    /// The decoder's answer doesn't fit what was asked.
    Answer(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Name(n) => write!(f, "{n:?} is not a file name in assets/"),
            Error::Open(e) => write!(f, "assets: {e}"),
            Error::Size(w, h) => write!(f, "a box of {w}x{h}: each side 1 to {MAX_SIDE}"),
            Error::Decoder(e) => write!(f, "image decoder: {e}"),
            Error::Answer(why) => write!(f, "image decoder's answer: {why}"),
        }
    }
}
impl std::error::Error for Error {}

/// How long a decode may take.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// Where the bundle's pictures are: `$ATRIUM_APP_ROOT/assets` (`/assets`
/// in the app's jail, where the bundle is).
pub fn assets_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("ATRIUM_APP_ROOT").unwrap_or_else(|| "/".into())).join("assets")
}

/// A name in `assets/`: one or more path parts of `A-Z a-z 0-9 . _ -`,
/// none of them `.` or `..`, joined by `/`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && name.split('/').all(|p| {
        !p.is_empty() && p != "." && p != ".." && p.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    })
}

/// The bundle's picture `name` (in `assets/`), fitted into `max_width` ×
/// `max_height`, by a decoder of its own.
pub fn image(name: &str, max_width: u32, max_height: u32) -> Result<Picture, Error> {
    let f = open(name)?;
    decode(f, max_width, max_height)
}

/// A picture from an open file (one the person picked, say), decoded the
/// same way.
pub fn decode(file: File, max_width: u32, max_height: u32) -> Result<Picture, Error> {
    sized(max_width, max_height)?;
    Decoder::new()?.decode(file, max_width, max_height)
}

fn sized(max_width: u32, max_height: u32) -> Result<(), Error> {
    if !(1..=MAX_SIDE).contains(&max_width) || !(1..=MAX_SIDE).contains(&max_height) {
        return Err(Error::Size(max_width, max_height));
    }
    Ok(())
}

fn open(name: &str) -> Result<File, Error> {
    if !valid_name(name) {
        return Err(Error::Name(name.into()));
    }
    File::open(assets_dir().join(name)).map_err(Error::Open)
}

/// One decoder for many pictures (a jail is started once, not per
/// picture). After a picture it couldn't answer for (a crash, a limit,
/// the time), it is done: make another.
pub struct Decoder {
    session: atrium_worker::Session,
}

impl Decoder {
    pub fn new() -> Result<Decoder, Error> {
        let mut session = atrium_worker::system("image", TIMEOUT).map_err(Error::Decoder)?;
        // The largest answer: a full box and the frame around it.
        session.set_max_answer((MAX_SIDE as usize * MAX_SIDE as usize * 4) + 64);
        Ok(Decoder { session })
    }

    /// As [`image`], on this decoder.
    pub fn image(&mut self, name: &str, max_width: u32, max_height: u32) -> Result<Picture, Error> {
        let f = open(name)?;
        self.decode(f, max_width, max_height)
    }

    /// As [`decode`], on this decoder.
    pub fn decode(&mut self, file: File, max_width: u32, max_height: u32) -> Result<Picture, Error> {
        sized(max_width, max_height)?;
        let p: Picture = self.session.call(&Decode { max_width, max_height }, Some(file)).map_err(Error::Decoder)?;
        check(&p, max_width, max_height)?;
        Ok(p)
    }
}

/// The answer fits the box and its bytes are its pixels.
pub fn check(p: &Picture, max_width: u32, max_height: u32) -> Result<(), Error> {
    if p.width > max_width || p.height > max_height {
        return Err(Error::Answer(format!("{}x{} is larger than the {max_width}x{max_height} asked for", p.width, p.height)));
    }
    let want = p.width as usize * p.height as usize * 4;
    if p.rgba.len() != want {
        return Err(Error::Answer(format!("{} bytes for {}x{}, not {want}", p.rgba.len(), p.width, p.height)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["app.svg", "icons/app.svg", "a_b-c.PNG"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "../x.png", "a/../b", "/abs.png", "a//b", "a b.png", "./x", "x\0.png"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn answers_are_checked() {
        let p = Picture { width: 2, height: 1, rgba: vec![0; 8] };
        assert!(check(&p, 2, 2).is_ok());
        assert!(check(&p, 1, 2).is_err());
        assert!(check(&Picture { rgba: vec![0; 7], ..p.clone() }, 2, 2).is_err());
        assert!(matches!(decode(File::open("/dev/null").unwrap(), 0, 5), Err(Error::Size(0, 5))));
        assert!(matches!(image("../etc/passwd", 5, 5), Err(Error::Name(_))));
    }

    #[test]
    fn messages_round_trip() {
        let d = Decode { max_width: 64, max_height: 48 };
        assert_eq!(Decode::from_frame(&d.to_frame(), 1 << 10).unwrap(), d);
        assert!(Decode::from_frame(&Decode { max_width: 0, max_height: 1 }.to_frame(), 1 << 10).is_err());
    }
}
