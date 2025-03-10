use std::{
    io::{stdout, Write},
    time::Duration,
};

use atuin_common::utils::{self, Escapable as _};
use crossterm::{
    cursor::SetCursorStyle,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        KeyboardEnhancementFlags, MouseEvent, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute, terminal,
};
use eyre::Result;
use futures_util::FutureExt;
use semver::Version;
use unicode_width::UnicodeWidthStr;

use atuin_client::{
    database::{current_context, Database},
    history::{store::HistoryStore, History, HistoryStats},
    settings::{ExitMode, FilterMode, KeymapMode, SearchMode, Settings},
};

use super::{
    cursor::Cursor,
    engines::{SearchEngine, SearchState},
    history_list::{HistoryList, ListState, PREFIX_LENGTH},
};

use crate::{command::client::search::engines, VERSION};

use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    prelude::*,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Paragraph, Tabs},
    Frame, Terminal, TerminalOptions, Viewport,
};

const TAB_TITLES: [&str; 2] = ["Search", "Inspect"];

pub enum InputAction {
    Accept(usize),
    Copy(usize),
    Delete(usize),
    ReturnOriginal,
    ReturnQuery,
    Continue,
    Redraw,
}

#[allow(clippy::struct_field_names)]
pub struct State {
    history_count: i64,
    update_needed: Option<Version>,
    results_state: ListState,
    switched_search_mode: bool,
    search_mode: SearchMode,
    results_len: usize,
    accept: bool,
    keymap_mode: KeymapMode,
    tab_index: usize,

    search: SearchState,
    engine: Box<dyn SearchEngine>,
}

#[derive(Clone, Copy)]
struct StyleState {
    compact: bool,
    invert: bool,
    inner_width: usize,
}

impl State {
    async fn query_results(&mut self, db: &mut dyn Database) -> Result<Vec<History>> {
        let results = self.engine.query(&self.search, db).await?;

        self.results_state.select(0);
        self.results_len = results.len();

        Ok(results)
    }

    fn handle_input<W>(
        &mut self,
        settings: &Settings,
        input: &Event,
        w: &mut W,
    ) -> Result<InputAction>
    where
        W: Write,
    {
        execute!(w, EnableMouseCapture)?;
        let r = match input {
            Event::Key(k) => self.handle_key_input(settings, k),
            Event::Mouse(m) => self.handle_mouse_input(*m),
            Event::Paste(d) => self.handle_paste_input(d),
            _ => InputAction::Continue,
        };
        execute!(w, DisableMouseCapture)?;
        Ok(r)
    }

    fn handle_mouse_input(&mut self, input: MouseEvent) -> InputAction {
        match input.kind {
            event::MouseEventKind::ScrollDown => {
                self.scroll_down(1);
            }
            event::MouseEventKind::ScrollUp => {
                self.scroll_up(1);
            }
            _ => {}
        }
        InputAction::Continue
    }

    fn handle_paste_input(&mut self, input: &str) -> InputAction {
        for i in input.chars() {
            self.search.input.insert(i);
        }
        InputAction::Continue
    }

    #[allow(clippy::too_many_lines)]
    #[allow(clippy::cognitive_complexity)]
    fn handle_key_input(&mut self, settings: &Settings, input: &KeyEvent) -> InputAction {
        if input.kind == event::KeyEventKind::Release {
            return InputAction::Continue;
        }

        let ctrl = input.modifiers.contains(KeyModifiers::CONTROL);
        let alt = input.modifiers.contains(KeyModifiers::ALT);

        // Use Ctrl-n instead of Alt-n?
        let modfr = if settings.ctrl_n_shortcuts { ctrl } else { alt };

        // Common actions
        macro_rules! do_exit {
            () => {
                return match settings.exit_mode {
                    ExitMode::ReturnOriginal => InputAction::ReturnOriginal,
                    ExitMode::ReturnQuery => InputAction::ReturnQuery,
                }
            };
        }

        // core input handling, common for all tabs
        match input.code {
            KeyCode::Char('c' | 'g') if ctrl => return InputAction::ReturnOriginal,
            KeyCode::Esc if self.keymap_mode == KeymapMode::VimInsert => {
                let _ = execute!(stdout(), SetCursorStyle::SteadyBlock);
                self.keymap_mode = KeymapMode::VimNormal;
                return InputAction::Continue;
            }
            KeyCode::Esc => {
                do_exit!();
            }
            KeyCode::Tab => {
                return InputAction::Accept(self.results_state.selected());
            }
            KeyCode::Char('o') if ctrl => {
                self.tab_index = (self.tab_index + 1) % TAB_TITLES.len();

                return InputAction::Continue;
            }

            _ => {}
        }

        // handle tab-specific input
        // todo: split out search
        match self.tab_index {
            0 => {}

            1 => {
                return super::inspector::input(
                    self,
                    settings,
                    self.results_state.selected(),
                    input,
                );
            }

            _ => panic!("invalid tab index on input"),
        }
        // reset the state, will be set to true later if user really did change it
        self.switched_search_mode = false;

        match input.code {
            KeyCode::Enter => {
                if settings.enter_accept {
                    self.accept = true;
                }

                return InputAction::Accept(self.results_state.selected());
            }
            KeyCode::Char('y') if ctrl => {
                return InputAction::Copy(self.results_state.selected());
            }
            KeyCode::Char(c @ '1'..='9') if modfr => {
                return c.to_digit(10).map_or(InputAction::Continue, |c| {
                    InputAction::Accept(self.results_state.selected() + c as usize)
                })
            }
            KeyCode::Left if ctrl => self
                .search
                .input
                .prev_word(&settings.word_chars, settings.word_jump_mode),
            KeyCode::Char('b') if alt => self
                .search
                .input
                .prev_word(&settings.word_chars, settings.word_jump_mode),
            KeyCode::Left => {
                self.search.input.left();
            }
            KeyCode::Char('b') if ctrl => {
                self.search.input.left();
            }
            KeyCode::Right if ctrl => self
                .search
                .input
                .next_word(&settings.word_chars, settings.word_jump_mode),
            KeyCode::Char('f') if alt => self
                .search
                .input
                .next_word(&settings.word_chars, settings.word_jump_mode),
            KeyCode::Right => self.search.input.right(),
            KeyCode::Char('f') if ctrl => self.search.input.right(),
            KeyCode::Char('a') if ctrl => self.search.input.start(),
            KeyCode::Home => self.search.input.start(),
            KeyCode::Char('e') if ctrl => self.search.input.end(),
            KeyCode::End => self.search.input.end(),
            KeyCode::Backspace if ctrl => self
                .search
                .input
                .remove_prev_word(&settings.word_chars, settings.word_jump_mode),
            KeyCode::Backspace => {
                self.search.input.back();
            }
            KeyCode::Delete if ctrl => self
                .search
                .input
                .remove_next_word(&settings.word_chars, settings.word_jump_mode),
            KeyCode::Delete => {
                self.search.input.remove();
            }
            KeyCode::Char('d') if ctrl => {
                if self.search.input.as_str().is_empty() {
                    return InputAction::ReturnOriginal;
                }
                self.search.input.remove();
            }
            KeyCode::Char('w') if ctrl => {
                // remove the first batch of whitespace
                while matches!(self.search.input.back(), Some(c) if c.is_whitespace()) {}
                while self.search.input.left() {
                    if self.search.input.char().unwrap().is_whitespace() {
                        self.search.input.right(); // found whitespace, go back right
                        break;
                    }
                    self.search.input.remove();
                }
            }
            KeyCode::Char('u') if ctrl => self.search.input.clear(),
            KeyCode::Char('r') if ctrl => {
                let filter_modes = if settings.workspaces && self.search.context.git_root.is_some()
                {
                    vec![
                        FilterMode::Global,
                        FilterMode::Host,
                        FilterMode::Session,
                        FilterMode::Directory,
                        FilterMode::Workspace,
                    ]
                } else {
                    vec![
                        FilterMode::Global,
                        FilterMode::Host,
                        FilterMode::Session,
                        FilterMode::Directory,
                    ]
                };

                let i = self.search.filter_mode as usize;
                let i = (i + 1) % filter_modes.len();
                self.search.filter_mode = filter_modes[i];
            }
            KeyCode::Char('s') if ctrl => {
                self.switched_search_mode = true;
                self.search_mode = self.search_mode.next(settings);
                self.engine = engines::engine(self.search_mode);
            }
            KeyCode::Down if !settings.invert && self.results_state.selected() == 0 => {
                do_exit!();
            }
            KeyCode::Up if settings.invert && self.results_state.selected() == 0 => {
                do_exit!();
            }
            KeyCode::Char('j')
                if !ctrl
                    && !settings.invert
                    && self.keymap_mode == KeymapMode::VimNormal
                    && self.results_state.selected() == 0 =>
            {
                do_exit!();
            }
            KeyCode::Char('k')
                if !ctrl
                    && settings.invert
                    && self.keymap_mode == KeymapMode::VimNormal
                    && self.results_state.selected() == 0 =>
            {
                do_exit!();
            }
            KeyCode::Char('k') if !ctrl && self.keymap_mode == KeymapMode::VimNormal => {
                self.scroll_up(1);
            }
            KeyCode::Char('j') if !ctrl && self.keymap_mode == KeymapMode::VimNormal => {
                self.scroll_down(1);
            }
            KeyCode::Down if !settings.invert => {
                self.scroll_down(1);
            }
            KeyCode::Up if settings.invert => {
                self.scroll_down(1);
            }
            KeyCode::Char('n' | 'j') if ctrl && !settings.invert => {
                self.scroll_down(1);
            }
            KeyCode::Char('n' | 'j') if ctrl && settings.invert => {
                self.scroll_up(1);
            }
            KeyCode::Up if !settings.invert => {
                self.scroll_up(1);
            }
            KeyCode::Down if settings.invert => {
                self.scroll_up(1);
            }
            KeyCode::Char('p' | 'k') if ctrl && !settings.invert => {
                self.scroll_up(1);
            }
            KeyCode::Char('p' | 'k') if ctrl && settings.invert => {
                self.scroll_down(1);
            }
            KeyCode::Char('l') if ctrl => {
                return InputAction::Redraw;
            }
            KeyCode::Char('i') if self.keymap_mode == KeymapMode::VimNormal => {
                let _ = execute!(stdout(), SetCursorStyle::BlinkingBlock);
                self.keymap_mode = KeymapMode::VimInsert;
            }
            KeyCode::Char(c) if self.keymap_mode != KeymapMode::VimNormal => {
                self.search.input.insert(c);
            }
            KeyCode::PageDown if !settings.invert => {
                let scroll_len = self.results_state.max_entries() - settings.scroll_context_lines;
                self.scroll_down(scroll_len);
            }
            KeyCode::PageDown if settings.invert => {
                let scroll_len = self.results_state.max_entries() - settings.scroll_context_lines;
                self.scroll_up(scroll_len);
            }
            KeyCode::PageUp if !settings.invert => {
                let scroll_len = self.results_state.max_entries() - settings.scroll_context_lines;
                self.scroll_up(scroll_len);
            }
            KeyCode::PageUp if settings.invert => {
                let scroll_len = self.results_state.max_entries() - settings.scroll_context_lines;
                self.scroll_down(scroll_len);
            }
            _ => {}
        };

        InputAction::Continue
    }

    fn scroll_down(&mut self, scroll_len: usize) {
        let i = self.results_state.selected().saturating_sub(scroll_len);
        self.results_state.select(i);
    }

    fn scroll_up(&mut self, scroll_len: usize) {
        let i = self.results_state.selected() + scroll_len;
        self.results_state.select(i.min(self.results_len - 1));
    }

    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::bool_to_int_with_if)]
    #[allow(clippy::too_many_lines)]
    fn draw(
        &mut self,
        f: &mut Frame,
        results: &[History],
        stats: Option<HistoryStats>,
        settings: &Settings,
    ) {
        let compact = match settings.style {
            atuin_client::settings::Style::Auto => f.size().height < 14,
            atuin_client::settings::Style::Compact => true,
            atuin_client::settings::Style::Full => false,
        };
        let invert = settings.invert;
        let border_size = if compact { 0 } else { 1 };
        let preview_width = f.size().width - 2;
        let preview_height = if settings.show_preview && self.tab_index == 0 {
            let longest_command = results
                .iter()
                .max_by(|h1, h2| h1.command.len().cmp(&h2.command.len()));
            longest_command.map_or(0, |v| {
                std::cmp::min(
                    settings.max_preview_height,
                    v.command
                        .split('\n')
                        .map(|line| {
                            (line.len() as u16 + preview_width - 1 - border_size)
                                / (preview_width - border_size)
                        })
                        .sum(),
                )
            }) + border_size * 2
        } else if compact || self.tab_index == 1 {
            0
        } else {
            1
        };
        let show_help = settings.show_help && (!compact || f.size().height > 1);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(0)
            .horizontal_margin(1)
            .constraints(
                if invert {
                    [
                        Constraint::Length(1 + border_size),               // input
                        Constraint::Min(1),                                // results list
                        Constraint::Length(preview_height),                // preview
                        Constraint::Length(1),                             // tabs
                        Constraint::Length(if show_help { 1 } else { 0 }), // header (sic)
                    ]
                } else {
                    [
                        Constraint::Length(if show_help { 1 } else { 0 }), // header
                        Constraint::Length(1),                             // tabs
                        Constraint::Min(1),                                // results list
                        Constraint::Length(1 + border_size),               // input
                        Constraint::Length(preview_height),                // preview
                    ]
                }
                .as_ref(),
            )
            .split(f.size());

        let input_chunk = if invert { chunks[0] } else { chunks[3] };
        let results_list_chunk = if invert { chunks[1] } else { chunks[2] };
        let preview_chunk = if invert { chunks[2] } else { chunks[4] };
        let tabs_chunk = if invert { chunks[3] } else { chunks[1] };
        let header_chunk = if invert { chunks[4] } else { chunks[0] };

        // TODO: this should be split so that we have one interactive search container that is
        // EITHER a search box or an inspector. But I'm not doing that now, way too much atm.
        // also allocate less 🙈
        let titles = TAB_TITLES.iter().copied().map(Line::from).collect();

        let tabs = Tabs::new(titles)
            .block(Block::default().borders(Borders::NONE))
            .select(self.tab_index)
            .style(Style::default())
            .highlight_style(Style::default().bold().white().on_black());

        f.render_widget(tabs, tabs_chunk);

        let style = StyleState {
            compact,
            invert,
            inner_width: input_chunk.width.into(),
        };

        let header_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(
                [
                    Constraint::Ratio(1, 5),
                    Constraint::Ratio(3, 5),
                    Constraint::Ratio(1, 5),
                ]
                .as_ref(),
            )
            .split(header_chunk);

        let title = self.build_title();
        f.render_widget(title, header_chunks[0]);

        let help = self.build_help();
        f.render_widget(help, header_chunks[1]);

        let stats_tab = self.build_stats();
        f.render_widget(stats_tab, header_chunks[2]);

        match self.tab_index {
            0 => {
                let results_list = Self::build_results_list(style, results);
                f.render_stateful_widget(results_list, results_list_chunk, &mut self.results_state);
            }

            1 => {
                super::inspector::draw(
                    f,
                    results_list_chunk,
                    &results[self.results_state.selected()],
                    &stats.expect("Drawing inspector, but no stats"),
                );

                // HACK: I'm following up with abstracting this into the UI container, with a
                // sub-widget for search + for inspector
                let feedback = Paragraph::new("The inspector is new - please give feedback (good, or bad) at https://forum.atuin.sh");
                f.render_widget(feedback, input_chunk);

                return;
            }

            _ => {
                panic!("invalid tab index");
            }
        }

        let input = self.build_input(style);
        f.render_widget(input, input_chunk);

        let preview =
            self.build_preview(results, compact, preview_width, preview_chunk.width.into());
        f.render_widget(preview, preview_chunk);

        let extra_width = UnicodeWidthStr::width(self.search.input.substring());

        let cursor_offset = if compact { 0 } else { 1 };
        f.set_cursor(
            // Put cursor past the end of the input text
            input_chunk.x + extra_width as u16 + PREFIX_LENGTH + 1 + cursor_offset,
            input_chunk.y + cursor_offset,
        );
    }

    fn build_title(&mut self) -> Paragraph {
        let title = if self.update_needed.is_some() {
            Paragraph::new(Text::from(Span::styled(
                format!("Atuin v{VERSION} - UPGRADE"),
                Style::default().add_modifier(Modifier::BOLD).fg(Color::Red),
            )))
        } else {
            Paragraph::new(Text::from(Span::styled(
                format!("Atuin v{VERSION}"),
                Style::default().add_modifier(Modifier::BOLD),
            )))
        };
        title.alignment(Alignment::Left)
    }

    #[allow(clippy::unused_self)]
    fn build_help(&self) -> Paragraph {
        match self.tab_index {
            // search
            0 => Paragraph::new(Text::from(Line::from(vec![
                Span::styled("<esc>", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(": exit"),
                Span::raw(", "),
                Span::styled("<tab>", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(": edit"),
                Span::raw(", "),
                Span::styled("<enter>", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(": run"),
                Span::raw(", "),
                Span::styled("<ctrl-o>", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(": inspect"),
            ]))),

            1 => Paragraph::new(Text::from(Line::from(vec![
                Span::styled("<esc>", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(": exit"),
                Span::raw(", "),
                Span::styled("<ctrl-o>", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(": search"),
                Span::raw(", "),
                Span::styled("<ctrl-d>", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(": delete"),
            ]))),

            _ => unreachable!("invalid tab index"),
        }
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center)
    }

    fn build_stats(&mut self) -> Paragraph {
        let stats = Paragraph::new(Text::from(Span::raw(format!(
            "history count: {}",
            self.history_count,
        ))))
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Right);
        stats
    }

    fn build_results_list(style: StyleState, results: &[History]) -> HistoryList {
        let results_list = HistoryList::new(results, style.invert);

        if style.compact {
            results_list
        } else if style.invert {
            results_list.block(
                Block::default()
                    .borders(Borders::LEFT | Borders::RIGHT)
                    .border_type(BorderType::Rounded)
                    .title(format!("{:─>width$}", "", width = style.inner_width - 2)),
            )
        } else {
            results_list.block(
                Block::default()
                    .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                    .border_type(BorderType::Rounded),
            )
        }
    }

    fn build_input(&mut self, style: StyleState) -> Paragraph {
        /// Max width of the UI box showing current mode
        const MAX_WIDTH: usize = 14;
        let (pref, mode) = if self.switched_search_mode {
            (" SRCH:", self.search_mode.as_str())
        } else {
            ("", self.search.filter_mode.as_str())
        };
        let mode_width = MAX_WIDTH - pref.len();
        // sanity check to ensure we don't exceed the layout limits
        debug_assert!(mode_width >= mode.len(), "mode name '{mode}' is too long!");
        let input = format!("[{pref}{mode:^mode_width$}] {}", self.search.input.as_str(),);
        let input = Paragraph::new(input);
        if style.compact {
            input
        } else if style.invert {
            input.block(
                Block::default()
                    .borders(Borders::LEFT | Borders::RIGHT | Borders::TOP)
                    .border_type(BorderType::Rounded),
            )
        } else {
            input.block(
                Block::default()
                    .borders(Borders::LEFT | Borders::RIGHT)
                    .border_type(BorderType::Rounded)
                    .title(format!("{:─>width$}", "", width = style.inner_width - 2)),
            )
        }
    }

    fn build_preview(
        &mut self,
        results: &[History],
        compact: bool,
        preview_width: u16,
        chunk_width: usize,
    ) -> Paragraph {
        let selected = self.results_state.selected();
        let command = if results.is_empty() {
            String::new()
        } else {
            use itertools::Itertools as _;
            let s = &results[selected].command;
            s.split('\n')
                .flat_map(|line| {
                    line.char_indices()
                        .step_by(preview_width.into())
                        .map(|(i, _)| i)
                        .chain(Some(line.len()))
                        .tuple_windows()
                        .map(|(a, b)| (&line[a..b]).escape_control().to_string())
                })
                .join("\n")
        };
        let preview = if compact {
            Paragraph::new(command).style(Style::default().fg(Color::DarkGray))
        } else {
            Paragraph::new(command).block(
                Block::default()
                    .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
                    .border_type(BorderType::Rounded)
                    .title(format!("{:─>width$}", "", width = chunk_width - 2)),
            )
        };
        preview
    }
}

struct Stdout {
    stdout: std::io::Stdout,
    inline_mode: bool,
}

impl Stdout {
    pub fn new(inline_mode: bool) -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = stdout();

        if !inline_mode {
            execute!(stdout, terminal::EnterAlternateScreen)?;
        }

        execute!(
            stdout,
            event::EnableMouseCapture,
            event::EnableBracketedPaste,
        )?;

        #[cfg(not(target_os = "windows"))]
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            ),
        )?;

        Ok(Self {
            stdout,
            inline_mode,
        })
    }
}

impl Drop for Stdout {
    fn drop(&mut self) {
        if !self.inline_mode {
            execute!(self.stdout, terminal::LeaveAlternateScreen).unwrap();
        }
        execute!(
            self.stdout,
            event::DisableMouseCapture,
            event::DisableBracketedPaste,
        )
        .unwrap();

        #[cfg(not(target_os = "windows"))]
        execute!(self.stdout, PopKeyboardEnhancementFlags).unwrap();

        terminal::disable_raw_mode().unwrap();
    }
}

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stdout.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stdout.flush()
    }
}

// this is a big blob of horrible! clean it up!
// for now, it works. But it'd be great if it were more easily readable, and
// modular. I'd like to add some more stats and stuff at some point
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
pub async fn history(
    query: &[String],
    settings: &Settings,
    mut db: impl Database,
    history_store: &HistoryStore,
) -> Result<String> {
    let stdout = Stdout::new(settings.inline_height > 0)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: if settings.inline_height > 0 {
                Viewport::Inline(settings.inline_height)
            } else {
                Viewport::Fullscreen
            },
        },
    )?;

    let mut input = Cursor::from(query.join(" "));
    // Put the cursor at the end of the query by default
    input.end();

    let settings2 = settings.clone();
    let update_needed = tokio::spawn(async move { settings2.needs_update().await }).fuse();
    tokio::pin!(update_needed);

    let context = current_context();

    let history_count = db.history_count(false).await?;
    let search_mode = if settings.shell_up_key_binding {
        settings
            .search_mode_shell_up_key_binding
            .unwrap_or(settings.search_mode)
    } else {
        settings.search_mode
    };
    let mut app = State {
        history_count,
        results_state: ListState::default(),
        update_needed: None,
        switched_search_mode: false,
        search_mode,
        tab_index: 0,
        search: SearchState {
            input,
            filter_mode: if settings.workspaces && context.git_root.is_some() {
                FilterMode::Workspace
            } else if settings.shell_up_key_binding {
                settings
                    .filter_mode_shell_up_key_binding
                    .unwrap_or(settings.filter_mode)
            } else {
                settings.filter_mode
            },
            context,
        },
        engine: engines::engine(search_mode),
        results_len: 0,
        accept: false,
        keymap_mode: match settings.keymap_mode {
            KeymapMode::Auto => KeymapMode::Emacs,
            value => value,
        },
    };

    let mut results = app.query_results(&mut db).await?;

    let mut stats: Option<HistoryStats> = None;
    let accept;
    let result = 'render: loop {
        terminal.draw(|f| app.draw(f, &results, stats.clone(), settings))?;

        let initial_input = app.search.input.as_str().to_owned();
        let initial_filter_mode = app.search.filter_mode;
        let initial_search_mode = app.search_mode;

        let event_ready = tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(250)));

        tokio::select! {
            event_ready = event_ready => {
                if event_ready?? {
                    loop {
                        match app.handle_input(settings, &event::read()?, &mut std::io::stdout())? {
                            InputAction::Continue => {},
                            InputAction::Delete(index) => {
                                app.results_len -= 1;
                                let selected = app.results_state.selected();
                                if selected == app.results_len {
                                    app.results_state.select(selected - 1);
                                }

                                let entry = results.remove(index);

                                if settings.sync.records {
                                    let (id, _) = history_store.delete(entry.id).await?;
                                    history_store.incremental_build(&db, &[id]).await?;
                                } else {
                                    db.delete(entry.clone()).await?;
                                }

                                app.tab_index  = 0;
                            },
                            InputAction::Redraw => {
                                terminal.clear()?;
                                terminal.draw(|f| app.draw(f, &results, stats.clone(), settings))?;
                            },
                            r => {
                                accept = app.accept;
                                break 'render r;
                            },
                        }
                        if !event::poll(Duration::ZERO)? {
                            break;
                        }
                    }
                }
            }
            update_needed = &mut update_needed => {
                app.update_needed = update_needed?;
            }
        }

        if initial_input != app.search.input.as_str()
            || initial_filter_mode != app.search.filter_mode
            || initial_search_mode != app.search_mode
        {
            results = app.query_results(&mut db).await?;
        }

        stats = if app.tab_index == 0 {
            None
        } else {
            let selected = results[app.results_state.selected()].clone();
            Some(db.stats(&selected).await?)
        };
    };

    if settings.inline_height > 0 {
        terminal.clear()?;
    }

    match result {
        InputAction::Accept(index) if index < results.len() => {
            let mut command = results.swap_remove(index).command;
            if accept && (utils::is_zsh() || utils::is_fish() || utils::is_bash()) {
                command = String::from("__atuin_accept__:") + &command;
            }

            // index is in bounds so we return that entry
            Ok(command)
        }
        InputAction::ReturnOriginal => Ok(String::new()),
        InputAction::Copy(index) => {
            let cmd = results.swap_remove(index).command;
            set_clipboard(cmd);
            Ok(String::new())
        }
        InputAction::ReturnQuery | InputAction::Accept(_) => {
            // Either:
            // * index == RETURN_QUERY, in which case we should return the input
            // * out of bounds -> usually implies no selected entry so we return the input
            Ok(app.search.input.into_inner())
        }
        InputAction::Continue | InputAction::Redraw | InputAction::Delete(_) => {
            unreachable!("should have been handled!")
        }
    }
}

#[cfg(feature = "clipboard")]
fn set_clipboard(s: String) {
    cli_clipboard::set_contents(s).unwrap();
}

#[cfg(not(feature = "clipboard"))]
fn set_clipboard(_s: String) {}
