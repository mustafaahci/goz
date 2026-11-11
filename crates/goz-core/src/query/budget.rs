//! How long a query may search, and how a search learns its time is up.
//!
//! The daemon searches a volume under its index read lock. The journal tail
//! waits for that lock, and every later query on the volume waits behind the
//! tail, so one slow query holds them all up. A query's cost grows with its
//! terms times the entries it tests, and a wildcard with no literal to scan
//! for tests every entry: one such query held a 4.6M-entry volume for 7 s,
//! and 256 of its terms for half an hour. The search loops and the wildcard
//! matcher check a [`Budget`] as they go and stop once it is spent; the
//! query is then refused, never answered in part.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// When a query must stop: at a deadline, or once its caller cancels it.
pub struct Budget<'a> {
    deadline: Option<Instant>,
    cancel: Option<&'a AtomicBool>,
    /// Set by the first check that finds the budget spent, so every thread
    /// of the search stops at its next check, and the query knows it stopped.
    spent: AtomicBool,
}

impl<'a> Budget<'a> {
    /// A budget that is never spent.
    pub const fn unlimited() -> Self {
        Self {
            deadline: None,
            cancel: None,
            spent: AtomicBool::new(false),
        }
    }

    /// A budget spent at `deadline`, or as soon as `cancel` is set.
    pub fn until(deadline: Instant, cancel: &'a AtomicBool) -> Self {
        Self {
            deadline: Some(deadline),
            cancel: Some(cancel),
            spent: AtomicBool::new(false),
        }
    }

    /// Whether a check found the budget spent, so the search stopped short.
    pub fn spent(&self) -> bool {
        self.spent.load(Ordering::Relaxed)
    }

    /// Checks the budget now, reading the clock. Out of the way of the loops
    /// that call it, which run it far less often than their own steps.
    #[cold]
    #[inline(never)]
    pub fn check(&self) -> bool {
        if self.spent() {
            return true;
        }
        let over = self.cancel.is_some_and(|c| c.load(Ordering::Relaxed))
            || self.deadline.is_some_and(|d| Instant::now() >= d);
        if over {
            self.spent.store(true, Ordering::Relaxed);
        }
        over
    }
}

/// How many items a [`Watch`] counts between checks of its budget. An item
/// costs about a millisecond at worst, as what can cost it more checks the
/// budget itself (see `GLOB_COSTLY_STEPS` and `FolderPaths::decide_needs`), so
/// a spent budget is noticed within about 200 ms, and the cheapest items, an
/// entry turned away by a filter in a few nanoseconds, pay a 24 ns clock
/// read in 256.
const ITEMS_PER_CHECK: u32 = 256;

/// One thread's watch on a [`Budget`], told of each item a search tests: an
/// entry, or a distinct name. An item only counts down: anything more showed
/// on a scan that turns each entry away in 5 ns.
pub(crate) struct Watch<'a> {
    budget: &'a Budget<'a>,
    left: u32,
}

impl<'a> Watch<'a> {
    pub(crate) fn new(budget: &'a Budget<'a>) -> Self {
        // The first item checks, so a budget spent already stops at once.
        Self { budget, left: 1 }
    }

    /// Counts one item. `true` once a check found the budget spent: the
    /// search then tests nothing more.
    #[inline]
    pub(crate) fn tick(&mut self) -> bool {
        self.left -= 1;
        self.left == 0 && self.check()
    }

    /// Once the budget is spent, every item comes here and hears so.
    #[cold]
    #[inline(never)]
    fn check(&mut self) -> bool {
        let spent = self.budget.check();
        self.left = if spent { 1 } else { ITEMS_PER_CHECK };
        spent
    }
}

/// A query its [`Budget`] stopped before it finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the query was stopped before it finished")]
pub struct Stopped;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn an_unlimited_budget_is_never_spent() {
        let budget = Budget::unlimited();
        let mut watch = Watch::new(&budget);
        assert!((0..1000).all(|_| !watch.tick()));
        assert!(!budget.check() && !budget.spent());
    }

    /// A cancel reaches each watch at its next check, and stays: the budget
    /// reports itself spent once one has seen it.
    #[test]
    fn a_cancel_reaches_every_watch_at_its_next_check() {
        let cancel = AtomicBool::new(false);
        let budget = Budget::until(Instant::now() + Duration::from_secs(3600), &cancel);
        let (mut first, mut second) = (Watch::new(&budget), Watch::new(&budget));
        assert!(!first.tick() && !second.tick());
        cancel.store(true, Ordering::Relaxed);
        assert!(!budget.spent(), "nothing checked yet");
        for watch in [&mut first, &mut second] {
            let ticks = (0..=ITEMS_PER_CHECK).take_while(|_| !watch.tick()).count();
            assert_eq!(
                ticks as u32,
                ITEMS_PER_CHECK - 1,
                "noticed at the next check"
            );
            assert!(watch.tick(), "and from then on");
        }
        assert!(budget.spent());
    }

    #[test]
    fn a_deadline_passed_already_stops_the_first_item() {
        let cancel = AtomicBool::new(false);
        let budget = Budget::until(Instant::now(), &cancel);
        assert!(Watch::new(&budget).tick());
        assert!(budget.spent());
    }
}
