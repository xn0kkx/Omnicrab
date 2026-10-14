//! The chat message box: text, cursor and prompt history, readline style.
//! Pure state with no terminal access, so it is easy to test.

const MAX_HISTORY: usize = 200;

#[derive(Clone, Debug, Default)]
pub struct InputBuf {
    text: String,
    /// Cursor position in chars (not bytes).
    cursor: usize,
    /// Submitted messages, oldest first. Kept in memory only: prompts are
    /// sensitive and everything persisted goes through the encrypted store.
    history: Vec<String>,
    /// Index into `history` while browsing with Up/Down.
    browsing: Option<usize>,
    /// What was being typed when browsing started.
    draft: String,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl InputBuf {
    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    fn chars(&self) -> Vec<char> {
        self.text.chars().collect()
    }

    /// Replaces the text (cursor to the end), e.g. after a completion.
    pub fn set(&mut self, s: impl Into<String>) {
        self.text = s.into();
        self.cursor = self.len();
        self.browsing = None;
    }

    pub fn clear(&mut self) {
        self.set("");
    }

    /// Takes the text for sending and records it in the history.
    pub fn take(&mut self) -> String {
        let t = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.browsing = None;
        let trimmed = t.trim();
        if !trimmed.is_empty() && self.history.last().map(String::as_str) != Some(trimmed) {
            self.history.push(trimmed.to_string());
            if self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
        }
        t
    }

    pub fn insert(&mut self, c: char) {
        let b = self.byte(self.cursor);
        self.text.insert(b, c);
        self.cursor += 1;
        self.browsing = None;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let b = self.byte(self.cursor - 1);
        self.text.remove(b);
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.len() {
            return;
        }
        let b = self.byte(self.cursor);
        self.text.remove(b);
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    /// Start of the line the cursor is on.
    fn line_start(&self) -> usize {
        let c = self.chars();
        let mut i = self.cursor;
        while i > 0 && c[i - 1] != '\n' {
            i -= 1;
        }
        i
    }

    fn line_end(&self) -> usize {
        let c = self.chars();
        let mut i = self.cursor;
        while i < c.len() && c[i] != '\n' {
            i += 1;
        }
        i
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    fn word_left_pos(&self) -> usize {
        let c = self.chars();
        let mut i = self.cursor;
        while i > 0 && !is_word(c[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(c[i - 1]) {
            i -= 1;
        }
        i
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_left_pos();
    }

    pub fn word_right(&mut self) {
        let c = self.chars();
        let mut i = self.cursor;
        while i < c.len() && !is_word(c[i]) {
            i += 1;
        }
        while i < c.len() && is_word(c[i]) {
            i += 1;
        }
        self.cursor = i;
    }

    fn remove_range(&mut self, from: usize, to: usize) {
        let (a, b) = (self.byte(from), self.byte(to));
        self.text.replace_range(a..b, "");
        self.cursor = from;
    }

    /// Ctrl+W / Alt+Backspace.
    pub fn delete_word_back(&mut self) {
        self.remove_range(self.word_left_pos(), self.cursor);
    }

    /// Ctrl+K.
    pub fn kill_to_end(&mut self) {
        let end = self.line_end();
        // At the end of a line, join with the next one.
        let end = if end == self.cursor && end < self.len() {
            end + 1
        } else {
            end
        };
        self.remove_range(self.cursor, end);
    }

    /// Ctrl+U.
    pub fn kill_to_start(&mut self) {
        self.remove_range(self.line_start(), self.cursor);
    }

    /// `(line, column)` of the cursor.
    pub fn line_col(&self) -> (usize, usize) {
        let before: String = self.text.chars().take(self.cursor).collect();
        let line = before.matches('\n').count();
        let col = before
            .rsplit('\n')
            .next()
            .map(|l| l.chars().count())
            .unwrap_or(0);
        (line, col)
    }

    fn line_count(&self) -> usize {
        self.text.split('\n').count()
    }

    /// Cursor position of `(line, col)`, clamped to the line length.
    fn pos_of(&self, line: usize, col: usize) -> usize {
        let mut pos = 0;
        for (i, l) in self.text.split('\n').enumerate() {
            let n = l.chars().count();
            if i == line {
                return pos + col.min(n);
            }
            pos += n + 1;
        }
        self.len()
    }

    /// Up arrow: previous line, or older history from the first line.
    pub fn up(&mut self) {
        let (line, col) = self.line_col();
        if line > 0 {
            self.cursor = self.pos_of(line - 1, col);
            return;
        }
        if self.history.is_empty() {
            return;
        }
        let next = match self.browsing {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.show_history(next);
    }

    /// Down arrow: next line, or newer history (and finally the draft) from the last line.
    pub fn down(&mut self) {
        let (line, col) = self.line_col();
        if line + 1 < self.line_count() {
            self.cursor = self.pos_of(line + 1, col);
            return;
        }
        match self.browsing {
            None => self.cursor = self.len(),
            Some(i) if i + 1 < self.history.len() => self.show_history(i + 1),
            Some(_) => {
                self.text = std::mem::take(&mut self.draft);
                self.cursor = self.len();
                self.browsing = None;
            }
        }
    }

    fn show_history(&mut self, i: usize) {
        self.text = self.history[i].clone();
        self.cursor = self.len();
        self.browsing = Some(i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> InputBuf {
        let mut b = InputBuf::default();
        s.chars().for_each(|c| b.insert(c));
        b
    }

    #[test]
    fn insert_and_delete_at_cursor() {
        let mut b = typed("héllo");
        b.left();
        b.left();
        b.insert('X');
        assert_eq!(b.as_str(), "hélXlo");
        b.backspace();
        b.delete();
        assert_eq!(b.as_str(), "hélo");
        assert_eq!(b.cursor(), 3);
    }

    #[test]
    fn home_end_and_words() {
        let mut b = typed("foo bar_baz  qux");
        b.word_left();
        assert_eq!(b.cursor(), 13);
        b.word_left();
        assert_eq!(b.cursor(), 4);
        b.word_right();
        assert_eq!(b.cursor(), 11);
        b.home();
        assert_eq!(b.cursor(), 0);
        b.end();
        assert_eq!(b.cursor(), 16);
        b.delete_word_back();
        assert_eq!(b.as_str(), "foo bar_baz  ");
    }

    #[test]
    fn kill_line_parts() {
        let mut b = typed("one\ntwo three");
        b.word_left();
        b.kill_to_start();
        assert_eq!(b.as_str(), "one\nthree");
        b.home();
        b.left();
        b.kill_to_end(); // at end of line 1: joins the next line
        assert_eq!(b.as_str(), "onethree");
    }

    #[test]
    fn arrows_move_between_lines_then_history() {
        let mut b = InputBuf::default();
        for m in ["first", "second"] {
            m.chars().for_each(|c| b.insert(c));
            b.take();
        }
        let mut b2 = b.clone();
        "draft".chars().for_each(|c| b2.insert(c));
        b2.up();
        assert_eq!(b2.as_str(), "second");
        b2.up();
        assert_eq!(b2.as_str(), "first");
        b2.up();
        assert_eq!(b2.as_str(), "first");
        b2.down();
        assert_eq!(b2.as_str(), "second");
        b2.down();
        assert_eq!(b2.as_str(), "draft");

        let mut m = typed("ab\ncdef");
        m.up();
        assert_eq!(m.line_col(), (0, 2));
        m.down();
        assert_eq!(m.line_col(), (1, 2));
    }

    #[test]
    fn history_skips_blank_and_duplicate() {
        let mut b = typed("same");
        b.take();
        "same".chars().for_each(|c| b.insert(c));
        b.take();
        "  ".chars().for_each(|c| b.insert(c));
        b.take();
        b.up();
        assert_eq!(b.as_str(), "same");
        b.up();
        assert_eq!(b.as_str(), "same");
    }
}
