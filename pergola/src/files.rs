//! Files (Scrinium), as an app asks it to show a file (scrinium.md §4.5).
//!
//! "Show in Files": the person's own Files window comes up on one of their
//! folders with the file selected. The app names the folder and the file's
//! own name, never a path, and learns nothing back but whether Files took
//! the ask: not the folder's contents, nor whether the file is still there.
//!
//! ```ignore
//! // In the button's handler: the press, which Files spends.
//! let press = pergola::clipboard::current_activation();
//! let ask = files::show(&embeds, token, SHOW_SLOT, Folder::Downloads, "report.pdf", press)?;
//! // then, for each event from embeds.poll():
//! if let Some(answer) = files::shown(&embeds, ask, &ev) { ... }
//! ```

use std::cell::RefCell;
use std::rc::Rc;

pub use limen::picker::Folder;

use crate::clipboard::Token;
use crate::embed::{EmbedId, Embeds, Error, ParentEvent, WindowToken};
use crate::slot::SlotKey;

/// Ask Files to show `name` in the person's `folder`, in this app's
/// persona, with `press`, the person's press on the app's button (Files
/// refuses without one). `slot` is never drawn: Files answers in a window
/// of its own.
pub fn show(embeds: &Rc<RefCell<Embeds>>, window: WindowToken, slot: SlotKey, folder: Folder, name: &str, press: Token) -> Result<EmbedId, Error> {
    let ask = limen::picker::ToChild::Show { folder, name: name.to_string(), press };
    if limen::picker::ToChild::decode(&ask.encode()).is_none() {
        return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, "a file's own name, with no folder in it (limen::picker::plain_name)")));
    }
    let mut e = embeds.borrow_mut();
    let embed = e.request("picker", window, slot)?;
    if let Err(err) = e.send(embed, ask.encode()) {
        let _ = e.detach_embed(embed);
        return Err(err);
    }
    Ok(embed)
}

/// Files' answer to the ask `embed`, if `ev` is it: `Ok` once its window
/// is asked for. The ask is detached by then.
pub fn shown(embeds: &Rc<RefCell<Embeds>>, embed: EmbedId, ev: &ParentEvent) -> Option<Result<(), String>> {
    let answer = match ev {
        ParentEvent::Message { embed: e, bytes } if *e == embed => match limen::picker::ToParent::decode(bytes) {
            Some(limen::picker::ToParent::Done { files: 0 }) => Ok(()),
            Some(limen::picker::ToParent::Failed { why }) => Err(why),
            _ => Err("an odd answer from Files".into()),
        },
        ParentEvent::ChildLost { embed: e } | ParentEvent::Detached { embed: e } | ParentEvent::Dropped { embed: e } if *e == embed =>
            Err("Files went away".into()),
        _ => return None,
    };
    let _ = embeds.borrow_mut().detach_embed(embed);
    Some(answer)
}
