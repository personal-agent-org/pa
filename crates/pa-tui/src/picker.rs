//! A small, reusable filter + cursor primitive for list popups.
//!
//! Several popups share the same shape: a typed query that narrows a list, plus a highlighted
//! row navigated with ↑/↓. `FilterList` owns just that interaction state — the query string and
//! the cursor — with bounded navigation helpers, so each popup supplies its own rows + rendering
//! while the keyboard behaviour (type to filter, arrows to move) stays consistent. The
//! integrations picker is the first consumer; future pickers can reuse it.

#[derive(Default)]
pub struct FilterList {
    /// The live filter text (case-insensitive substring match — see `matches`).
    pub query: String,
    /// Highlighted row within the CURRENT (filtered) result list.
    pub cursor: usize,
}

impl FilterList {
    pub fn new() -> FilterList {
        FilterList {
            query: String::new(),
            cursor: 0,
        }
    }

    /// Clear the query and reset the cursor (on open).
    pub fn reset(&mut self) {
        self.query.clear();
        self.cursor = 0;
    }

    /// Move the highlight up one row (saturating at the top).
    pub fn up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the highlight down one row, clamped to the last of `len` rows.
    pub fn down(&mut self, len: usize) {
        if self.cursor + 1 < len {
            self.cursor += 1;
        }
    }

    /// Append a character to the query and snap the highlight back to the top (the result set
    /// just changed, so the old index is meaningless).
    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.cursor = 0;
    }

    /// Delete the last query character (no-op on an empty query); reset the highlight.
    pub fn backspace(&mut self) {
        self.query.pop();
        self.cursor = 0;
    }

    /// Whether `hay` matches the current query (case-insensitive substring; empty query = all).
    pub fn matches(&self, hay: &str) -> bool {
        let q = self.query.trim().to_lowercase();
        q.is_empty() || hay.to_lowercase().contains(&q)
    }

    /// Clamp the cursor into `[0, len)` (call after the result set shrinks, e.g. a fresh load).
    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_is_bounded() {
        let mut f = FilterList::new();
        // Up at the top stays at 0.
        f.up();
        assert_eq!(f.cursor, 0);
        // Down stops at len-1.
        f.down(3);
        f.down(3);
        f.down(3);
        f.down(3);
        assert_eq!(f.cursor, 2);
        // An empty list never advances.
        let mut g = FilterList::new();
        g.down(0);
        assert_eq!(g.cursor, 0);
    }

    #[test]
    fn typing_filters_and_resets_cursor() {
        let mut f = FilterList::new();
        f.down(5);
        assert_eq!(f.cursor, 1);
        f.push('a');
        // A query edit snaps the highlight back to the top.
        assert_eq!(f.cursor, 0);
        assert_eq!(f.query, "a");
        f.backspace();
        assert_eq!(f.query, "");
    }

    #[test]
    fn matches_is_case_insensitive_substring() {
        let mut f = FilterList::new();
        assert!(f.matches("anything")); // empty query matches all
        f.push('S');
        f.push('e');
        assert!(f.matches("web_search"));
        assert!(!f.matches("weather"));
    }

    #[test]
    fn clamp_keeps_cursor_in_range() {
        let mut f = FilterList::new();
        f.down(10);
        f.down(10);
        assert_eq!(f.cursor, 2);
        f.clamp(2); // list shrank to 2 rows → last valid index is 1
        assert_eq!(f.cursor, 1);
        f.clamp(0); // empty → 0
        assert_eq!(f.cursor, 0);
    }
}
