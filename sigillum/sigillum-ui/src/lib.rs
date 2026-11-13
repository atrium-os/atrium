//! sigillum-ui: Sigillum's trusted sheets (docs/spec/sigillum.md §7.2,
//! §7.3). It draws what `sigillumd` tells it, spends the user's press with
//! frescod (purpose `IDENTITY`) before passing a choice on, and takes PINs
//! and passcodes in its own window, so no app ever sees them.

pub mod gate;
pub mod keymap;
pub mod sheet;
pub mod view;
