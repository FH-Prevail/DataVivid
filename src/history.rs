//! Undo/redo as a list of applied operations with the table after each.
//! Polars columns are reference-counted, so a snapshot only costs memory for
//! the columns an operation actually changed.

use polars::prelude::DataFrame;

use crate::ops::Op;

const MAX_STEPS: usize = 200;

pub struct Step {
    pub op: Op,
    pub df: DataFrame,
}

pub struct History {
    base: DataFrame,
    steps: Vec<Step>,
    /// Number of applied steps (steps after it are redo-able).
    cursor: usize,
    /// Cursor value at the last save; `None` if that state was dropped.
    saved_at: Option<usize>,
    /// Bumped on every change, so caches know when to refresh.
    version: u64,
    /// Steps folded into `base` because of `MAX_STEPS`.
    folded: usize,
}

impl History {
    pub fn new(df: DataFrame) -> Self {
        History { base: df, steps: Vec::new(), cursor: 0, saved_at: Some(0), version: 0, folded: 0 }
    }

    pub fn current(&self) -> &DataFrame {
        if self.cursor == 0 { &self.base } else { &self.steps[self.cursor - 1].df }
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn push(&mut self, op: Op, df: DataFrame) {
        self.steps.truncate(self.cursor);
        if let Some(s) = self.saved_at {
            if s > self.cursor {
                self.saved_at = None;
            }
        }
        self.steps.push(Step { op, df });
        self.cursor += 1;
        if self.steps.len() > MAX_STEPS {
            let first = self.steps.remove(0);
            self.base = first.df;
            self.cursor -= 1;
            self.folded += 1;
            self.saved_at = match self.saved_at {
                Some(0) | None => None,
                Some(s) => Some(s - 1),
            };
        }
        self.version += 1;
    }

    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }
    pub fn can_redo(&self) -> bool {
        self.cursor < self.steps.len()
    }
    pub fn undo(&mut self) -> Option<&Op> {
        if !self.can_undo() {
            return None;
        }
        self.cursor -= 1;
        self.version += 1;
        Some(&self.steps[self.cursor].op)
    }
    pub fn redo(&mut self) -> Option<&Op> {
        if !self.can_redo() {
            return None;
        }
        self.cursor += 1;
        self.version += 1;
        Some(&self.steps[self.cursor - 1].op)
    }
    /// Jumps to the state after `n` applied steps (0 = as loaded).
    pub fn jump_to(&mut self, n: usize) {
        let n = n.min(self.steps.len());
        if n != self.cursor {
            self.cursor = n;
            self.version += 1;
        }
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn folded(&self) -> usize {
        self.folded
    }

    pub fn is_dirty(&self) -> bool {
        self.saved_at != Some(self.cursor)
    }
    pub fn mark_saved(&mut self) {
        self.saved_at = Some(self.cursor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polars::prelude::*;

    fn df(n: i64) -> DataFrame {
        df!("a" => [n]).unwrap()
    }
    fn val(h: &History) -> i64 {
        h.current().column("a").unwrap().i64().unwrap().get(0).unwrap()
    }
    fn op() -> Op {
        Op::Sql { query: "x".into() }
    }

    #[test]
    fn undo_redo_and_dirty() {
        let mut h = History::new(df(0));
        assert!(!h.is_dirty());
        h.push(op(), df(1));
        h.push(op(), df(2));
        assert!(h.is_dirty());
        h.undo();
        assert_eq!(val(&h), 1);
        h.redo();
        assert_eq!(val(&h), 2);
        h.mark_saved();
        assert!(!h.is_dirty());
        h.undo();
        assert!(h.is_dirty());
        h.push(op(), df(9)); // drops redo, saved state is gone
        assert_eq!(val(&h), 9);
        assert!(!h.can_redo());
        h.undo();
        h.undo();
        assert_eq!(val(&h), 0);
        assert!(h.is_dirty());
        h.jump_to(2);
        assert_eq!(val(&h), 9);
    }
}
