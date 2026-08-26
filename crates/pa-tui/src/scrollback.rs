//! Deciding which transcript lines may be printed into the terminal's scrollback.
//!
//! The chat no longer redraws itself every frame in an alternate screen. Finished lines are
//! written ABOVE the inline viewport with `Terminal::insert_before`, into the terminal's own
//! scrollback, so selecting, copying, scrolling and searching are the terminal's job again and
//! the transcript survives quitting (personal-agent-org/personal-agent#126).
//!
//! The price is that printing is final: a line in the scrollback can never be revised. So the
//! only interesting question in this module is **which lines are finished**, and the answer has
//! exactly two rules:
//!
//! * Every line of a message that is no longer pending is finished.
//! * Of a pending message, every line except the last is finished — the last one is still
//!   growing, so it lives in the viewport until a newline arrives behind it.
//!
//! Getting that wrong is not a cosmetic bug. Committing one line too many leaves a half-written
//! sentence in the scrollback that nothing can go back and complete.

use ratatui::text::Line;

/// How many lines of the in-flight turn the viewport shows while it grows.
pub const LIVE_ROWS: usize = 3;

/// What the caller should do with the rendered transcript this frame.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Emission {
    /// Lines to hand to `insert_before`, in order. Never revised afterwards.
    pub commit: Vec<usize>,
    /// Lines that belong in the viewport because they may still change.
    pub live: Vec<usize>,
}

/// Split a rendered transcript into what may be committed and what must stay live.
///
/// `rendered` is the whole transcript as lines; `finished_upto` is how many of its lines belong
/// to messages that are settled, and `already` how many were committed in earlier frames.
///
/// Indices rather than lines, so this stays a decision and the caller keeps ownership of the
/// rendering. It also makes the rules testable without constructing styled text.
pub fn split(rendered_len: usize, finished_upto: usize, already: usize) -> Emission {
    let finished = finished_upto.min(rendered_len);
    // A shrinking transcript means the caller re-rendered something already printed (a chat
    // switch, a resize that changed wrapping). Committing "the rest" would duplicate lines that
    // are already in the scrollback, so nothing is committed until it grows past the mark again.
    let commit: Vec<usize> = (already.min(finished)..finished).collect();
    let live: Vec<usize> = (finished..rendered_len).take(LIVE_ROWS).collect();
    Emission { commit, live }
}

/// The separator printed when the view moves to a different chat.
///
/// The scrollback is append-only: the previous chat's transcript cannot be taken back, and
/// pretending otherwise (by clearing) would throw away the very history this change exists to
/// keep. So the new chat is announced and printed below the old one, the way a terminal
/// session accumulates.
pub fn chat_separator(title: &str, width: usize) -> Vec<Line<'static>> {
    let label = if title.trim().is_empty() {
        " chat ".to_string()
    } else {
        format!(" {} ", title.trim())
    };
    // The rule fills whatever the label leaves; label + rule is exactly `width`. A title
    // longer than the terminal leaves nothing, which is fine -- it must not underflow.
    let rule = width.saturating_sub(label.chars().count());
    let left = rule / 2;
    let right = rule - left;
    vec![
        Line::from(""),
        Line::from(format!(
            "{}{}{}",
            "─".repeat(left),
            label,
            "─".repeat(right)
        )),
        Line::from(""),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settled_transcript_commits_everything_once() {
        let e = split(10, 10, 0);
        assert_eq!(e.commit, (0..10).collect::<Vec<_>>());
        assert!(e.live.is_empty());
        // …and a second frame with nothing new commits nothing.
        assert_eq!(split(10, 10, 10), Emission::default());
    }

    #[test]
    fn the_growing_last_line_is_never_committed() {
        // The rule that matters: 8 finished lines of a pending turn, the 9th still growing.
        let e = split(9, 8, 0);
        assert_eq!(e.commit, (0..8).collect::<Vec<_>>());
        assert_eq!(e.live, vec![8]);
    }

    #[test]
    fn a_line_is_committed_exactly_once_as_the_turn_grows() {
        let mut committed = 0;
        let mut seen = Vec::new();
        // Frame by frame: the turn grows one finished line at a time.
        for (len, finished) in [(1, 0), (2, 1), (3, 2), (4, 3)] {
            let e = split(len, finished, committed);
            seen.extend(e.commit.iter().copied());
            committed += e.commit.len();
        }
        assert_eq!(seen, vec![0, 1, 2]);
        // Nothing appeared twice — which in the terminal would be a duplicated line nothing
        // can remove.
        let mut uniq = seen.clone();
        uniq.dedup();
        assert_eq!(seen, uniq);
    }

    #[test]
    fn a_shrinking_transcript_commits_nothing() {
        // Re-rendering narrower re-wraps and can yield FEWER lines than were already printed.
        // The printed ones are gone from our reach; emitting "the rest" would duplicate them.
        assert!(split(4, 4, 9).commit.is_empty());
    }

    #[test]
    fn the_live_window_is_bounded() {
        // A turn that emits a large block at once must not push the composer off the screen.
        let e = split(100, 0, 0);
        assert_eq!(e.live.len(), LIVE_ROWS);
        assert!(e.commit.is_empty());
    }

    #[test]
    fn finished_beyond_the_end_is_clamped() {
        // Defensive: the caller computes both numbers, and disagreeing about the length must
        // not index past the transcript.
        let e = split(3, 99, 0);
        assert_eq!(e.commit, vec![0, 1, 2]);
        assert!(e.live.is_empty());
    }

    #[test]
    fn the_separator_fills_the_width() {
        let lines = chat_separator("Deployment", 40);
        let text: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text.chars().count(), 40);
        assert!(text.contains("Deployment"));
    }

    #[test]
    fn an_untitled_chat_still_gets_a_rule() {
        let lines = chat_separator("   ", 20);
        let text: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text.chars().count(), 20);
    }

    #[test]
    fn a_narrow_terminal_does_not_panic() {
        // The rule arithmetic subtracts the label width; a title longer than the terminal
        // must not underflow.
        let lines = chat_separator("ein sehr langer Chattitel", 10);
        assert_eq!(lines.len(), 3);
    }
}
