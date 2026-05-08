//! Reactive primitives — the bridge between async data flow and the
//! sync UI thread.
//!
//! The toolkit's public API is sync: `View::render` reads values
//! synchronously and produces a node tree. Reactivity comes from
//! **observable cells** (`Mutable<T>` from `futures-signals`) that
//! mark the app dirty on `.set()`. The app's main loop checks the
//! dirty flag, re-renders when set, and diffs vs the previous tree
//! to produce minimal deltas.
//!
//! See `docs/spec/pergola.md` and `docs/design/atrium-visual-language.md`
//! §6 for the philosophy: sync API + signal bridge, no
//! async-await-everywhere.
//!
//! ## Usage
//!
//! ```ignore
//! use pergola::reactive::Mutable;
//!
//! struct Counter { count: Mutable<i32> }
//!
//! impl View for Counter {
//!     fn render(&self, ctx: &mut Ctx) {
//!         let n = self.count.get();   // sync read
//!         // ... emit nodes that depend on `n` ...
//!     }
//! }
//!
//! // Elsewhere (event handler, async task, anywhere):
//! counter.count.set(counter.count.get() + 1);   // sync write, fires reactivity
//! ```
//!
//! `Mutable<T>` is a thin wrapper around `futures_signals::signal::Mutable<T>`
//! with a sync `get()` for `T: Copy + 'static` and a sync `get_cloned()` for
//! `T: Clone + 'static`. Sync reads mean view code stays simple.

use futures_signals::signal::Mutable as FsMutable;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// An observable cell. Reads are sync; writes also trip the global
/// app dirty flag (if one was registered) so the next event-loop tick
/// re-renders.
///
/// `T` must be `'static` because the underlying `futures-signals` cell
/// holds the value behind a lock that may outlive any single
/// borrow.
#[derive(Debug, Clone)]
pub struct Mutable<T: 'static> {
    inner: FsMutable<T>,
    dirty: Option<Arc<AtomicBool>>,
}

impl<T: 'static> Mutable<T> {
    /// Construct a Mutable not yet attached to any app dirty flag.
    /// Useful for tests and for cells that exist before an `App` does.
    pub fn new(value: T) -> Self {
        Self { inner: FsMutable::new(value), dirty: None }
    }

    /// Construct a Mutable wired to an `App`'s dirty flag. Each `set`
    /// will flip the flag.
    pub fn with_dirty(value: T, dirty: Arc<AtomicBool>) -> Self {
        Self { inner: FsMutable::new(value), dirty: Some(dirty) }
    }

    /// Attach this Mutable to an app dirty flag *after* construction.
    /// Returns a clone with the flag applied so callers can use a
    /// builder-style pattern.
    pub fn attach(mut self, dirty: Arc<AtomicBool>) -> Self {
        self.dirty = Some(dirty);
        self
    }

    /// Sync write. Marks dirty, replaces the value.
    pub fn set(&self, value: T) {
        self.inner.set(value);
        if let Some(d) = &self.dirty {
            d.store(true, Ordering::Release);
        }
    }
}

impl<T: Copy + 'static> Mutable<T> {
    /// Sync read of the current value. Cheap — copies.
    pub fn get(&self) -> T { self.inner.get() }
}

impl<T: Clone + 'static> Mutable<T> {
    /// Sync read by clone. Use when `T` isn't `Copy` (e.g. `String`).
    pub fn get_cloned(&self) -> T { self.inner.get_cloned() }
}

/// Re-export the underlying signal types for advanced use cases.
/// Most apps don't need these directly — they read via `get`/`get_cloned`
/// inside `View::render`.
pub use futures_signals::signal::{Signal, SignalExt};
