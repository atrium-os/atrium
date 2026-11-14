//! Undo and redo (docs/spec/officina.md §6).
//!
//! An op's `apply` changes the document and returns the op that undoes it. A user
//! action is one transaction of one or more ops; undo applies the inverses in reverse
//! order and the inverses of those become the redo transaction.

/// Something that changes a document `D` and returns its own inverse.
pub trait Op<D>: Sized {
    fn apply(self, doc: &mut D) -> Self;
}

/// One user action.
#[derive(Debug, Clone)]
pub struct Transaction<O> {
    pub label: String,
    pub ops: Vec<O>,
}

/// The undo and redo stacks.
#[derive(Debug)]
pub struct History<O> {
    undo: Vec<Transaction<O>>,
    redo: Vec<Transaction<O>>,
    /// The most transactions kept for undo; the oldest go first.
    pub limit: usize,
}

impl<O> Default for History<O> {
    fn default() -> Self {
        History { undo: Vec::new(), redo: Vec::new(), limit: 1000 }
    }
}

impl<O> History<O> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies `ops` as one action labelled `label`. Clears redo.
    pub fn run<D>(&mut self, doc: &mut D, label: &str, ops: Vec<O>)
    where
        O: Op<D>,
    {
        let inv: Vec<O> = ops.into_iter().map(|o| o.apply(doc)).collect();
        self.redo.clear();
        self.undo.push(Transaction { label: label.to_string(), ops: inv });
        if self.undo.len() > self.limit {
            let extra = self.undo.len() - self.limit;
            self.undo.drain(..extra);
        }
    }

    pub fn undo<D>(&mut self, doc: &mut D) -> Option<String>
    where
        O: Op<D>,
    {
        let t = self.undo.pop()?;
        // Apply the inverses last first; collect before reversing so the order of application
        // is really reversed.
        let mut redo: Vec<O> = t.ops.into_iter().rev().map(|o| o.apply(doc)).collect();
        redo.reverse();
        // Redo replays forward: the inverses of the undo ops, in the original order.
        let label = t.label.clone();
        self.redo.push(Transaction { label: t.label, ops: redo });
        Some(label)
    }

    pub fn redo<D>(&mut self, doc: &mut D) -> Option<String>
    where
        O: Op<D>,
    {
        let t = self.redo.pop()?;
        let undo: Vec<O> = t.ops.into_iter().map(|o| o.apply(doc)).collect();
        let label = t.label.clone();
        self.undo.push(Transaction { label: t.label, ops: undo });
        Some(label)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|t| t.label.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A toy document: a vector of numbers; the op sets one slot.
    struct Set(usize, i32);
    impl Op<Vec<i32>> for Set {
        fn apply(self, d: &mut Vec<i32>) -> Self {
            let old = std::mem::replace(&mut d[self.0], self.1);
            Set(self.0, old)
        }
    }

    #[test]
    fn undo_redo() {
        let mut d = vec![0; 3];
        let mut h = History::new();
        h.run(&mut d, "a", vec![Set(0, 1), Set(0, 2), Set(1, 5)]);
        h.run(&mut d, "b", vec![Set(2, 9)]);
        assert_eq!(d, [2, 5, 9]);
        assert_eq!(h.undo(&mut d).as_deref(), Some("b"));
        assert_eq!(d, [2, 5, 0]);
        assert_eq!(h.undo(&mut d).as_deref(), Some("a"));
        assert_eq!(d, [0, 0, 0]);
        assert!(h.undo(&mut d).is_none());
        h.redo(&mut d);
        assert_eq!(d, [2, 5, 0]);
        h.redo(&mut d);
        assert_eq!(d, [2, 5, 9]);
        h.undo(&mut d);
        h.run(&mut d, "c", vec![Set(1, 7)]);
        assert!(!h.can_redo());
        assert_eq!(d, [2, 7, 0]);
    }
}
