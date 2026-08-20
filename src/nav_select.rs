//! Minimal navigable select menu with level navigation on arrow keys.
//!
//! inquire's `Select` owns Left/Right for its filter-input cursor and exposes no
//! custom keybinding API, so the interactive session manager uses this widget
//! instead: Left goes back one level, Right (or Enter) selects the highlighted
//! option. Key handling lives in the pure `NavState` so it is testable without a
//! TTY; only `NavSelect::prompt` touches the terminal.

use std::io::Write;

use anyhow::{Context, Result};
use colored::Colorize;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::ClearType;
use crossterm::{cursor, execute, terminal};
use unicode_width::UnicodeWidthChar;

/// What the user did with the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavOutcome {
    /// An option was chosen (Enter or Right). The index refers to the original
    /// `options` vector, not the filtered view.
    Selected(usize),
    /// Left arrow: go back one navigation level.
    Back,
    /// Esc or Ctrl-C: cancel the menu (callers keep their legacy mapping).
    Cancel,
}

/// Builder-style select prompt. See module docs for the key map.
pub struct NavSelect {
    prompt: String,
    options: Vec<String>,
    page_size: usize,
    help_message: Option<String>,
}

const DEFAULT_PAGE_SIZE: usize = 15;
const DEFAULT_HELP: &str = "↑↓ navigate · → or Enter select · ← back · type to filter · Esc cancel";

impl NavSelect {
    pub fn new(prompt: impl Into<String>, options: Vec<String>) -> Self {
        Self {
            prompt: prompt.into(),
            options,
            page_size: DEFAULT_PAGE_SIZE,
            help_message: None,
        }
    }

    #[allow(dead_code)]
    pub fn with_page_size(mut self, page_size: usize) -> Self {
        self.page_size = page_size.max(1);
        self
    }

    #[allow(dead_code)]
    pub fn with_help_message(mut self, message: impl Into<String>) -> Self {
        self.help_message = Some(message.into());
        self
    }

    /// Run the menu. Empty option lists cancel immediately without entering raw
    /// mode; `Err` is reserved for real terminal I/O failures.
    pub fn prompt(self) -> Result<NavOutcome> {
        if self.options.is_empty() {
            return Ok(NavOutcome::Cancel);
        }

        // Keep the whole frame shorter than the terminal so MoveUp-based
        // redrawing cannot scroll history away on small windows.
        let rows = terminal::size().map(|(_, r)| r as usize).unwrap_or(24);
        let page_size = self.page_size.min(rows.saturating_sub(4)).max(3);

        let help = self
            .help_message
            .unwrap_or_else(|| DEFAULT_HELP.to_string());
        let mut state = NavState::new(self.options, page_size);
        let mut stdout = std::io::stdout();

        let guard = RawModeGuard::enable()?;
        let mut drawn_lines: u16 = 0;
        let outcome = loop {
            drawn_lines = render(&mut stdout, &self.prompt, &help, &state, drawn_lines)?;
            match crossterm::event::read().context("failed to read terminal event")? {
                Event::Key(key) => {
                    if let Some(outcome) = state.handle_key(key) {
                        break outcome;
                    }
                }
                // Next render() erases with the stale line count first; a full
                // FromCursorDown clear below the frame start keeps it sane.
                Event::Resize(_, _) => {}
                _ => {}
            }
        };
        erase(&mut stdout, drawn_lines)?;
        drop(guard);

        if let NavOutcome::Selected(index) = outcome {
            println!(
                "{} {} {}",
                "?".green(),
                self.prompt,
                state.options[index].trim().cyan()
            );
        }
        Ok(outcome)
    }
}

/// Pure menu state: options, filter, cursor. No terminal I/O.
struct NavState {
    options: Vec<String>,
    /// Lowercased labels, precomputed for case-insensitive filtering.
    lowered: Vec<String>,
    filter: String,
    /// Indices into `options` that match the current filter.
    filtered: Vec<usize>,
    /// Cursor position within `filtered`.
    cursor: usize,
    /// Scroll offset within `filtered` (first visible row).
    offset: usize,
    page_size: usize,
}

impl NavState {
    fn new(options: Vec<String>, page_size: usize) -> Self {
        let lowered = options.iter().map(|o| o.to_lowercase()).collect();
        let filtered = (0..options.len()).collect();
        Self {
            options,
            lowered,
            filter: String::new(),
            filtered,
            cursor: 0,
            offset: 0,
            page_size: page_size.max(1),
        }
    }

    /// Apply one key press. `Some(outcome)` ends the menu; `None` means the
    /// state changed (or the key was ignored) and the menu re-renders.
    fn handle_key(&mut self, key: KeyEvent) -> Option<NavOutcome> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => Some(NavOutcome::Cancel),
                _ => None,
            };
        }

        let last = self.filtered.len().saturating_sub(1);
        match key.code {
            KeyCode::Up => {
                if !self.filtered.is_empty() {
                    self.cursor = if self.cursor == 0 {
                        last
                    } else {
                        self.cursor - 1
                    };
                }
            }
            KeyCode::Down => {
                if !self.filtered.is_empty() {
                    self.cursor = if self.cursor == last {
                        0
                    } else {
                        self.cursor + 1
                    };
                }
            }
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(self.page_size),
            KeyCode::PageDown => self.cursor = (self.cursor + self.page_size).min(last),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = last,
            KeyCode::Enter | KeyCode::Right => {
                if let Some(&original) = self.filtered.get(self.cursor) {
                    return Some(NavOutcome::Selected(original));
                }
            }
            KeyCode::Left => return Some(NavOutcome::Back),
            KeyCode::Esc => return Some(NavOutcome::Cancel),
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.refilter();
            }
            KeyCode::Backspace => {
                if self.filter.pop().is_some() {
                    self.refilter();
                }
            }
            _ => {}
        }
        self.clamp_scroll();
        None
    }

    /// Recompute the filtered view, keeping the cursor on the same original
    /// option when it is still visible, otherwise clamping into range.
    fn refilter(&mut self) {
        let followed = self.filtered.get(self.cursor).copied();
        let needle = self.filter.to_lowercase();
        self.filtered = (0..self.options.len())
            .filter(|&i| self.lowered[i].contains(&needle))
            .collect();
        self.cursor = followed
            .and_then(|original| self.filtered.iter().position(|&i| i == original))
            .unwrap_or_else(|| self.cursor.min(self.filtered.len().saturating_sub(1)));
    }

    /// Keep the scroll window over the cursor.
    fn clamp_scroll(&mut self) {
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + self.page_size {
            self.offset = self.cursor + 1 - self.page_size;
        }
        let max_offset = self.filtered.len().saturating_sub(self.page_size);
        self.offset = self.offset.min(max_offset);
    }

    /// The `[start, end)` range of `filtered` currently visible.
    fn visible_window(&self) -> (usize, usize) {
        let end = (self.offset + self.page_size).min(self.filtered.len());
        (self.offset, end)
    }
}

/// Truncate to a display width (CJK chars count as 2 columns), appending `…`
/// when the string was cut. Guarantees the result fits within `max_cols`, which
/// the renderer relies on to keep one option per terminal row.
fn truncate_to_width(text: &str, max_cols: usize) -> String {
    let total: usize = text.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= max_cols {
        return text.to_string();
    }

    let budget = max_cols.saturating_sub(1); // reserve one column for the ellipsis
    let mut used = 0;
    let mut result = String::new();
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        result.push(c);
    }
    result.push('…');
    result
}

/// Restores the terminal (raw mode off, cursor shown) even on panic.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self> {
        terminal::enable_raw_mode().context("failed to enable raw terminal mode")?;
        let _ = execute!(std::io::stdout(), cursor::Hide);
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), cursor::Show);
    }
}

/// Erase the previously drawn frame (cursor sits just below it).
fn erase(out: &mut impl Write, drawn_lines: u16) -> Result<()> {
    if drawn_lines > 0 {
        execute!(
            out,
            cursor::MoveToColumn(0),
            cursor::MoveUp(drawn_lines),
            terminal::Clear(ClearType::FromCursorDown)
        )?;
    }
    Ok(())
}

/// Draw one frame and return the number of lines written.
fn render(
    out: &mut impl Write,
    prompt: &str,
    help: &str,
    state: &NavState,
    previous_lines: u16,
) -> Result<u16> {
    erase(out, previous_lines)?;
    let cols = terminal::size().map(|(c, _)| c as usize).unwrap_or(80);
    let width = cols.saturating_sub(1).max(10);
    let mut lines: u16 = 0;

    let header = format!("? {} {}", prompt, state.filter);
    write!(
        out,
        "{}\r\n",
        truncate_to_width(header.trim_end(), width).bold()
    )?;
    lines += 1;

    let (start, end) = state.visible_window();
    if state.filtered.is_empty() {
        write!(out, "{}\r\n", "  (no matches)".dimmed())?;
        lines += 1;
    }
    if start > 0 {
        write!(out, "{}\r\n", "  ▲ more".dimmed())?;
        lines += 1;
    }
    for (row, &original) in state.filtered[start..end].iter().enumerate() {
        let label = truncate_to_width(&state.options[original], width.saturating_sub(2));
        if start + row == state.cursor {
            write!(out, "{} {}\r\n", ">".cyan().bold(), label.cyan())?;
        } else {
            write!(out, "  {label}\r\n")?;
        }
        lines += 1;
    }
    if end < state.filtered.len() {
        write!(out, "{}\r\n", "  ▼ more".dimmed())?;
        lines += 1;
    }

    write!(out, "{}\r\n", truncate_to_width(help, width).dimmed())?;
    lines += 1;
    out.flush()?;
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn state(labels: &[&str], page_size: usize) -> NavState {
        NavState::new(labels.iter().map(|s| s.to_string()).collect(), page_size)
    }

    fn abc_state() -> NavState {
        state(&["alpha", "beta", "gamma", "delta"], 15)
    }

    #[test]
    fn up_at_top_wraps_to_last_and_down_at_bottom_wraps_to_first() {
        let mut s = abc_state();
        assert_eq!(s.handle_key(key(KeyCode::Up)), None);
        assert_eq!(s.cursor, 3);
        assert_eq!(s.handle_key(key(KeyCode::Down)), None);
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn up_down_moves_within_bounds() {
        let mut s = abc_state();
        s.handle_key(key(KeyCode::Down));
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.cursor, 2);
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn page_down_and_page_up_clamp_without_wrapping() {
        let labels: Vec<String> = (0..10).map(|i| format!("item-{i}")).collect();
        let mut s = NavState::new(labels, 4);
        s.handle_key(key(KeyCode::PageDown));
        assert_eq!(s.cursor, 4);
        s.handle_key(key(KeyCode::PageDown));
        s.handle_key(key(KeyCode::PageDown));
        assert_eq!(s.cursor, 9, "PageDown clamps at the last option");
        s.handle_key(key(KeyCode::PageUp));
        assert_eq!(s.cursor, 5);
        s.handle_key(key(KeyCode::PageUp));
        s.handle_key(key(KeyCode::PageUp));
        assert_eq!(s.cursor, 0, "PageUp clamps at the first option");
    }

    #[test]
    fn home_and_end_jump_to_extremes() {
        let mut s = abc_state();
        s.handle_key(key(KeyCode::End));
        assert_eq!(s.cursor, 3);
        s.handle_key(key(KeyCode::Home));
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn filter_narrows_case_insensitively() {
        let mut s = state(&["Alpha", "beta", "ALPINE", "gamma"], 15);
        for c in "aL".chars() {
            assert_eq!(s.handle_key(key(KeyCode::Char(c))), None);
        }
        assert_eq!(s.filtered, vec![0, 2], "matches Alpha and ALPINE");
    }

    #[test]
    fn selected_returns_original_index_after_filter() {
        let mut s = state(&["zero", "one", "match-a", "three", "match-b"], 15);
        for c in "match".chars() {
            s.handle_key(key(KeyCode::Char(c)));
        }
        s.handle_key(key(KeyCode::Down));
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(NavOutcome::Selected(4)),
            "second visible item maps back to original index 4"
        );
    }

    #[test]
    fn filter_follows_cursor_item_when_still_visible() {
        let mut s = state(&["apple", "banana", "apricot"], 15);
        s.handle_key(key(KeyCode::Down));
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.filtered[s.cursor], 2, "cursor on apricot");
        s.handle_key(key(KeyCode::Char('a')));
        s.handle_key(key(KeyCode::Char('p')));
        assert_eq!(s.filtered, vec![0, 2]);
        assert_eq!(s.filtered[s.cursor], 2, "cursor still on apricot");
    }

    #[test]
    fn filter_clamps_cursor_when_item_disappears() {
        let mut s = state(&["apple", "banana", "cherry"], 15);
        s.handle_key(key(KeyCode::End));
        s.handle_key(key(KeyCode::Char('a')));
        assert_eq!(s.filtered, vec![0, 1]);
        assert!(s.cursor < s.filtered.len());
    }

    #[test]
    fn backspace_widens_filter_and_empty_backspace_is_noop() {
        let mut s = state(&["apple", "banana"], 15);
        s.handle_key(key(KeyCode::Char('a')));
        s.handle_key(key(KeyCode::Char('p')));
        assert_eq!(s.filtered, vec![0]);
        s.handle_key(key(KeyCode::Backspace));
        assert_eq!(s.filtered, vec![0, 1], "'a' matches both again");
        s.handle_key(key(KeyCode::Backspace));
        assert_eq!(s.handle_key(key(KeyCode::Backspace)), None);
        assert_eq!(s.filter, "");
        assert_eq!(s.filtered, vec![0, 1]);
    }

    #[test]
    fn enter_and_right_are_noops_on_empty_filtered_view() {
        let mut s = state(&["apple"], 15);
        s.handle_key(key(KeyCode::Char('z')));
        assert!(s.filtered.is_empty());
        assert_eq!(s.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(s.handle_key(key(KeyCode::Right)), None);
    }

    #[test]
    fn left_returns_back_even_while_filtering() {
        let mut s = abc_state();
        assert_eq!(s.handle_key(key(KeyCode::Left)), Some(NavOutcome::Back));
        let mut s = abc_state();
        s.handle_key(key(KeyCode::Char('a')));
        assert_eq!(s.handle_key(key(KeyCode::Left)), Some(NavOutcome::Back));
    }

    #[test]
    fn right_and_enter_both_select_highlighted_option() {
        let mut s = abc_state();
        s.handle_key(key(KeyCode::Down));
        assert_eq!(
            s.handle_key(key(KeyCode::Right)),
            Some(NavOutcome::Selected(1))
        );
        let mut s = abc_state();
        s.handle_key(key(KeyCode::Down));
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(NavOutcome::Selected(1))
        );
    }

    #[test]
    fn esc_and_ctrl_c_cancel() {
        let mut s = abc_state();
        assert_eq!(s.handle_key(key(KeyCode::Esc)), Some(NavOutcome::Cancel));
        let mut s = abc_state();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(s.handle_key(ctrl_c), Some(NavOutcome::Cancel));
    }

    #[test]
    fn release_key_events_are_ignored() {
        use crossterm::event::KeyEventState;
        let mut s = abc_state();
        let release = KeyEvent {
            code: KeyCode::Left,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert_eq!(s.handle_key(release), None);
    }

    #[test]
    fn empty_options_prompt_cancels_without_terminal() {
        let outcome = NavSelect::new("pick:", Vec::new()).prompt().unwrap();
        assert_eq!(outcome, NavOutcome::Cancel);
    }

    #[test]
    fn visible_window_scrolls_to_keep_cursor_visible() {
        let labels: Vec<String> = (0..10).map(|i| format!("item-{i}")).collect();
        let mut s = NavState::new(labels, 5);
        assert_eq!(s.visible_window(), (0, 5));
        for _ in 0..7 {
            s.handle_key(key(KeyCode::Down));
        }
        let (start, end) = s.visible_window();
        assert!(
            (start..end).contains(&s.cursor),
            "cursor {} must stay inside window {start}..{end}",
            s.cursor
        );
        assert_eq!(end - start, 5);
        s.handle_key(key(KeyCode::Home));
        assert_eq!(s.visible_window(), (0, 5), "window scrolls back up");
    }

    #[test]
    fn truncate_keeps_short_strings_and_cuts_wide_ones() {
        assert_eq!(truncate_to_width("abc", 10), "abc");
        assert_eq!(truncate_to_width("abcdef", 6), "abcdef");
        assert_eq!(truncate_to_width("abcdefg", 6), "abcde…");
        // CJK chars occupy 2 columns: 7 columns fit 3 chars (6 cols) + ellipsis.
        assert_eq!(truncate_to_width("会话标题测试", 7), "会话标…");
        assert_eq!(truncate_to_width("会话", 4), "会话");
    }
}
