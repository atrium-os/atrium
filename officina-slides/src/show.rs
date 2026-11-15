//! Presenting (officina.md §9.1, §9.3): where a slideshow is and what the
//! next press does. A slide's builds fall into steps: the ones before its
//! first on-click build play as the slide arrives, and each on-click build
//! starts a step of its own, joined by the with-previous and after-previous
//! builds behind it. The show walks steps and slides and says what to
//! animate; the app turns that into frescod animations between slides it
//! keeps resident.

use crate::model::{Build, Damaged, Deck, Effect, ShapeId, Transition, Trigger};

/// A build placed in its step: it starts `start_ms` after the step does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Planned {
    /// Index into the slide's builds.
    pub build: usize,
    pub start_ms: u32,
}

/// The slide's builds as steps; the first plays on arrival (and may be
/// empty).
pub fn steps(builds: &[Build]) -> Vec<Vec<Planned>> {
    let mut out: Vec<Vec<Planned>> = vec![Vec::new()];
    let (mut prev_start, mut prev_end) = (0u32, 0u32);
    for (i, b) in builds.iter().enumerate() {
        let start = match b.trigger {
            Trigger::OnClick => {
                out.push(Vec::new());
                b.delay_ms
            }
            Trigger::WithPrevious => prev_start.saturating_add(b.delay_ms),
            Trigger::AfterPrevious => prev_end.saturating_add(b.delay_ms),
        };
        prev_start = start;
        prev_end = start.saturating_add(b.ms);
        out.last_mut().expect("a step").push(Planned { build: i, start_ms: start });
    }
    out
}

/// How long a step runs, ms.
pub fn step_ms(builds: &[Build], step: &[Planned]) -> u32 {
    step.iter().map(|p| p.start_ms.saturating_add(builds[p.build].ms)).max().unwrap_or(0)
}

/// Whether the effect brings its shape in (else it takes it away).
pub fn enters(e: Effect) -> bool {
    !matches!(e, Effect::Disappear | Effect::FadeOut)
}

/// Whether `shape` shows once the first `done` steps have played.
pub fn shown(builds: &[Build], steps: &[Vec<Planned>], shape: ShapeId, done: usize) -> bool {
    let mine = |b: &&Build| b.shape == shape;
    let Some(first) = builds.iter().find(mine) else { return true };
    let mut on = !enters(first.effect);
    for step in steps.iter().take(done) {
        for p in step {
            let b = &builds[p.build];
            if b.shape == shape {
                on = enters(b.effect);
            }
        }
    }
    on
}

/// The shapes a slide's builds move: each is drawn as a part of its own.
pub fn moved(builds: &[Build]) -> Vec<ShapeId> {
    let mut v: Vec<ShapeId> = builds.iter().map(|b| b.shape).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// What a press (or a timer, or a jump) does.
#[derive(Debug, Clone, PartialEq)]
pub enum Go {
    /// Play step `step` of slide `slide`.
    Step { slide: usize, step: usize },
    /// Show slide `to` (with its first step, if any), from `from`, with
    /// `transition` (`None`: at once).
    Slide { from: Option<usize>, to: usize, transition: Option<Transition> },
    /// Show slide `slide` with only its first `done` steps played, at once
    /// (going back).
    Back { slide: usize, done: usize },
    /// Past the last slide: the black end screen. Pressing again ends.
    End,
    /// Nothing happens.
    Stay,
}

/// Where a show is.
#[derive(Debug, Clone, PartialEq)]
pub struct Show {
    /// The slide on screen (deck index).
    pub slide: usize,
    /// Steps played on it.
    pub done: usize,
    /// On the end screen.
    pub ended: bool,
    count: usize,
}

fn builds_of(deck: &mut Deck, i: usize) -> Result<Vec<Build>, Damaged> {
    Ok(deck.slide(i)?.builds.clone())
}

impl Show {
    /// A show of `deck`, not started.
    pub fn new(deck: &Deck) -> Show {
        Show { slide: 0, done: 0, ended: false, count: deck.slide_count() }
    }

    /// The first shown slide at or after `i` (hidden ones skipped), or
    /// `None`.
    fn shown_from(deck: &mut Deck, mut i: usize, back: bool) -> Option<usize> {
        let n = deck.slide_count();
        loop {
            if i >= n {
                return None;
            }
            // A slide that can't be read is shown (as its error), not skipped.
            if !deck.slide(i).map(|s| s.hidden).unwrap_or(false) {
                return Some(i);
            }
            if back {
                i = i.checked_sub(1)?;
            } else {
                i += 1;
            }
        }
    }

    /// Start at slide `at` (the next shown one).
    pub fn start(&mut self, deck: &mut Deck, at: usize) -> Go {
        self.count = deck.slide_count();
        let Some(to) = Self::shown_from(deck, at, false).or_else(|| Self::shown_from(deck, at.min(self.count.saturating_sub(1)), true)) else {
            self.ended = true;
            return Go::End;
        };
        self.enter(to);
        Go::Slide { from: None, to, transition: None }
    }

    fn enter(&mut self, to: usize) {
        self.slide = to;
        self.done = 1;
        self.ended = false;
    }

    /// The next step, else the next slide, else the end screen.
    pub fn next(&mut self, deck: &mut Deck) -> Go {
        if self.ended {
            return Go::Stay;
        }
        let steps = builds_of(deck, self.slide).map(|b| steps(&b).len()).unwrap_or(1);
        if self.done < steps {
            self.done += 1;
            return Go::Step { slide: self.slide, step: self.done - 1 };
        }
        match Self::shown_from(deck, self.slide + 1, false) {
            Some(to) => {
                let transition = deck.slide(to).ok().and_then(|s| s.transition);
                let from = self.slide;
                self.enter(to);
                Go::Slide { from: Some(from), to, transition }
            }
            None => {
                self.ended = true;
                Go::End
            }
        }
    }

    /// Back a step (shown at once), else the previous slide with all its
    /// steps played.
    pub fn prev(&mut self, deck: &mut Deck) -> Go {
        if self.ended {
            self.ended = false;
            return Go::Back { slide: self.slide, done: self.done };
        }
        if self.done > 1 {
            self.done -= 1;
            return Go::Back { slide: self.slide, done: self.done };
        }
        let Some(to) = self.slide.checked_sub(1).and_then(|i| Self::shown_from(deck, i, true)) else { return Go::Stay };
        self.slide = to;
        self.done = builds_of(deck, to).map(|b| steps(&b).len()).unwrap_or(1);
        Go::Back { slide: to, done: self.done }
    }

    /// Jump to slide `i` (from the presenter's sorter, or a number typed),
    /// hidden or not, at once.
    pub fn go_to(&mut self, deck: &mut Deck, i: usize) -> Go {
        if i >= deck.slide_count() {
            return Go::Stay;
        }
        let from = (!self.ended).then_some(self.slide);
        self.enter(i);
        Go::Slide { from, to: i, transition: None }
    }

    /// The slides to keep resident around the one shown (officina.md
    /// §9.3): it, the next and the previous shown ones.
    pub fn resident(&self, deck: &mut Deck) -> Vec<usize> {
        let mut v = vec![self.slide];
        if let Some(n) = Self::shown_from(deck, self.slide + 1, false) {
            v.push(n);
        }
        if let Some(p) = self.slide.checked_sub(1).and_then(|i| Self::shown_from(deck, i, true)) {
            v.push(p);
        }
        v
    }

    /// Slides in the deck.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Dir, TransitionKind};

    fn b(shape: u32, effect: Effect, trigger: Trigger, ms: u32, delay_ms: u32) -> Build {
        Build { shape: shape, effect, trigger, ms, delay_ms, by_paragraph: false }
    }

    #[test]
    fn builds_fall_into_steps_with_their_start_times() {
        let builds = [
            b(1, Effect::Fade, Trigger::WithPrevious, 500, 0),
            b(2, Effect::FlyIn(Dir::Left), Trigger::OnClick, 400, 100),
            b(3, Effect::Fade, Trigger::WithPrevious, 300, 50),
            b(4, Effect::Appear, Trigger::AfterPrevious, 0, 200),
            b(2, Effect::FadeOut, Trigger::OnClick, 500, 0),
        ];
        let s = steps(&builds);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], vec![Planned { build: 0, start_ms: 0 }]);
        assert_eq!(s[1], vec![Planned { build: 1, start_ms: 100 }, Planned { build: 2, start_ms: 150 }, Planned { build: 3, start_ms: 650 }]);
        assert_eq!(step_ms(&builds, &s[1]), 650);
        // Shape 2 comes in on the first click and goes on the second; shape
        // 1 shows once the slide arrives; shape 9 has no builds.
        assert!(!shown(&builds, &s, 2, 1));
        assert!(shown(&builds, &s, 2, 2));
        assert!(!shown(&builds, &s, 2, 3));
        assert!(!shown(&builds, &s, 1, 0) && shown(&builds, &s, 1, 1));
        assert!(shown(&builds, &s, 9, 0));
        assert_eq!(moved(&builds), vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_show_walks_steps_and_skips_hidden_slides() {
        let mut deck = Deck::new();
        let l = deck.layout_of_kind("obj").unwrap().id;
        for _ in 0..4 {
            let s = crate::ops::new_slide(&mut deck, l);
            deck.push_slide(s);
        }
        let ids: Vec<_> = (0..4).map(|i| deck.slide(i).unwrap().shapes.first().map(|s| s.id)).collect();
        let tr = Transition { kind: TransitionKind::Push(Dir::Left), ms: 700, on_click: true, after_ms: None };
        {
            let s = deck.slide_mut(0).unwrap();
            s.builds = vec![b(ids[0].unwrap(), Effect::Fade, Trigger::OnClick, 300, 0)];
        }
        deck.slide_mut(1).unwrap().hidden = true;
        deck.slide_mut(2).unwrap().transition = Some(tr);
        let mut show = Show::new(&deck);
        assert_eq!(show.start(&mut deck, 0), Go::Slide { from: None, to: 0, transition: None });
        assert_eq!(show.next(&mut deck), Go::Step { slide: 0, step: 1 });
        assert_eq!(show.next(&mut deck), Go::Slide { from: Some(0), to: 2, transition: Some(tr) }, "the hidden slide skipped");
        assert_eq!(show.resident(&mut deck), vec![2, 3, 0]);
        assert_eq!(show.prev(&mut deck), Go::Back { slide: 0, done: 2 }, "back to the slide with its build played");
        assert_eq!(show.prev(&mut deck), Go::Back { slide: 0, done: 1 });
        assert_eq!(show.prev(&mut deck), Go::Stay);
        assert_eq!(show.go_to(&mut deck, 1), Go::Slide { from: Some(0), to: 1, transition: None }, "a jump shows a hidden slide");
        assert!(matches!(show.next(&mut deck), Go::Slide { to: 2, .. }));
        assert!(matches!(show.next(&mut deck), Go::Slide { to: 3, .. }));
        assert_eq!(show.next(&mut deck), Go::End);
        assert_eq!(show.next(&mut deck), Go::Stay);
        assert_eq!(show.prev(&mut deck), Go::Back { slide: 3, done: 1 });
    }
}
