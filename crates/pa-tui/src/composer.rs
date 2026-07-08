//! Composer text-buffer editing — cursor-aware, UTF-8-safe, multiline.
//!
//! Pure functions over `(text, cursor)` where `cursor` is a BYTE index into `text` that
//! always sits on a char boundary. Kept free-standing (not methods on `App`) so the editing
//! logic is unit-tested in isolation — the TUI itself can't be driven headlessly.

/// Byte index of the char boundary just before `cursor` (or 0 at the start).
fn prev_index(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .chars()
        .next_back()
        .map(|c| cursor - c.len_utf8())
        .unwrap_or(0)
}

/// Byte index of the char boundary just after `cursor` (or `text.len()` at the end).
fn next_index(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .chars()
        .next()
        .map(|c| cursor + c.len_utf8())
        .unwrap_or(cursor)
}

/// Byte index where the current line begins (after the previous '\n', or 0).
fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Byte index where the current line ends (at the next '\n', or `text.len()`).
fn line_end(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .find('\n')
        .map(|i| cursor + i)
        .unwrap_or(text.len())
}

/// The byte index of the `col`-th char (0-based) within the line `[start, end)`, clamped to `end`.
fn col_index(text: &str, start: usize, end: usize, col: usize) -> usize {
    let mut idx = start;
    for (n, c) in text[start..end].chars().enumerate() {
        if n >= col {
            break;
        }
        idx += c.len_utf8();
    }
    idx.min(end)
}

pub fn insert_char(text: &mut String, cursor: &mut usize, c: char) {
    text.insert(*cursor, c);
    *cursor += c.len_utf8();
}

#[allow(dead_code)] // for bracketed-paste insertion (next slice)
pub fn insert_str(text: &mut String, cursor: &mut usize, s: &str) {
    text.insert_str(*cursor, s);
    *cursor += s.len();
}

/// Delete the char before the cursor; returns false at the start of the buffer.
pub fn backspace(text: &mut String, cursor: &mut usize) -> bool {
    if *cursor == 0 {
        return false;
    }
    let prev = prev_index(text, *cursor);
    text.replace_range(prev..*cursor, "");
    *cursor = prev;
    true
}

/// Delete the char at the cursor (forward delete).
pub fn delete(text: &mut String, cursor: &mut usize) {
    if *cursor >= text.len() {
        return;
    }
    let next = next_index(text, *cursor);
    text.replace_range(*cursor..next, "");
}

pub fn left(text: &str, cursor: &mut usize) {
    *cursor = prev_index(text, *cursor);
}

pub fn right(text: &str, cursor: &mut usize) {
    *cursor = next_index(text, *cursor);
}

pub fn home(text: &str, cursor: &mut usize) {
    *cursor = line_start(text, *cursor);
}

pub fn end(text: &str, cursor: &mut usize) {
    *cursor = line_end(text, *cursor);
}

/// Move the cursor up one visual line, keeping the column. Returns false if already on the
/// first line (so the caller can fall back to prompt history).
pub fn move_up(text: &str, cursor: &mut usize) -> bool {
    let ls = line_start(text, *cursor);
    if ls == 0 {
        return false;
    }
    let col = text[ls..*cursor].chars().count();
    let prev_end = ls - 1; // the '\n' terminating the previous line
    let prev_start = text[..prev_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
    *cursor = col_index(text, prev_start, prev_end, col);
    true
}

/// Move the cursor down one visual line, keeping the column. Returns false on the last line.
pub fn move_down(text: &str, cursor: &mut usize) -> bool {
    let le = line_end(text, *cursor);
    if le == text.len() {
        return false;
    }
    let ls = line_start(text, *cursor);
    let col = text[ls..*cursor].chars().count();
    let next_start = le + 1;
    let next_end = text[next_start..]
        .find('\n')
        .map(|i| next_start + i)
        .unwrap_or(text.len());
    *cursor = col_index(text, next_start, next_end, col);
    true
}

/// Delete the word before the cursor (Ctrl+W): skip trailing spaces, then the word.
pub fn delete_word_back(text: &mut String, cursor: &mut usize) {
    let mut i = *cursor;
    let is_ws =
        |s: &str, at: usize, to: usize| s[at..to].chars().next().is_some_and(char::is_whitespace);
    while i > 0 {
        let p = prev_index(text, i);
        if is_ws(text, p, i) {
            i = p;
        } else {
            break;
        }
    }
    while i > 0 {
        let p = prev_index(text, i);
        if !is_ws(text, p, i) {
            i = p;
        } else {
            break;
        }
    }
    text.replace_range(i..*cursor, "");
    *cursor = i;
}

/// Delete from the line start to the cursor (Ctrl+U).
pub fn kill_line_back(text: &mut String, cursor: &mut usize) {
    let ls = line_start(text, *cursor);
    text.replace_range(ls..*cursor, "");
    *cursor = ls;
}

/// (row, col) of the cursor — 0-based line and char-column within that line. For rendering.
#[allow(dead_code)] // exercised by the vertical-movement tests; handy for future scroll-to-cursor
pub fn row_col(text: &str, cursor: usize) -> (usize, usize) {
    let row = text[..cursor].matches('\n').count();
    let ls = line_start(text, cursor);
    let col = text[ls..cursor].chars().count();
    (row, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace_ascii() {
        let (mut t, mut c) = (String::new(), 0);
        for ch in "abc".chars() {
            insert_char(&mut t, &mut c, ch);
        }
        assert_eq!((t.as_str(), c), ("abc", 3));
        left(&t, &mut c); // between b and c
        insert_char(&mut t, &mut c, 'X');
        assert_eq!(t, "abXc");
        assert!(backspace(&mut t, &mut c));
        assert_eq!(t, "abc");
        assert_eq!(c, 2);
    }

    #[test]
    fn utf8_safe() {
        let (mut t, mut c) = (String::from("über"), 0);
        end(&t, &mut c);
        assert_eq!(c, t.len());
        assert!(backspace(&mut t, &mut c)); // drop 'r'
        left(&t, &mut c); // over 'e' boundary
                          // 'ü' is 2 bytes — left/backspace must never split it
        assert!(backspace(&mut t, &mut c));
        assert_eq!(t, "üe");
    }

    #[test]
    fn delete_word_back_skips_spaces_then_word() {
        let (mut t, mut c) = (String::from("hello world  "), 0);
        end(&t, &mut c);
        delete_word_back(&mut t, &mut c);
        assert_eq!(t, "hello ");
        delete_word_back(&mut t, &mut c);
        assert_eq!(t, "");
    }

    #[test]
    fn kill_line_back_only_current_line() {
        let (mut t, mut c) = (String::from("one\ntwo three"), 0);
        move_down(&t, &mut c); // onto the second line
        end(&t, &mut c); // end of THAT line (line-aware)
        kill_line_back(&mut t, &mut c);
        assert_eq!(t, "one\n");
    }

    #[test]
    fn vertical_movement_keeps_column_and_signals_edges() {
        let text = String::from("abcd\nef\nghij");
        // start on line 0, col 3 (between c and d)
        let mut c = 3;
        assert!(!move_up(&text, &mut c)); // first line → no move, signal history
        assert!(move_down(&text, &mut c)); // → line 1, clamped to col 2 (len of "ef")
        assert_eq!(row_col(&text, c), (1, 2));
        // No goal-column memory: the clamped col 2 carries forward (predictable, simple).
        assert!(move_down(&text, &mut c)); // → line 2, col 2
        assert_eq!(row_col(&text, c), (2, 2));
        assert!(!move_down(&text, &mut c)); // last line → no move
    }

    #[test]
    fn newline_insert_and_rowcol() {
        let (mut t, mut c) = (String::from("ab"), 2);
        insert_char(&mut t, &mut c, '\n');
        insert_char(&mut t, &mut c, 'c');
        assert_eq!(t, "ab\nc");
        assert_eq!(row_col(&t, c), (1, 1));
    }
}
