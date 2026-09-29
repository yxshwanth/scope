use std::error::Error;
use std::fs::File;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use code_scope::{Symbol, index};
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use crate::live::{LiveIndex, Update};

struct App {
    root: PathBuf,
    symbols: Vec<Symbol>,
    matches: Vec<usize>,
    query: String,
    query_error: Option<String>,
    list: ListState,
    preview: Result<Vec<String>, String>,
    offset: usize,
    status: String,
    busy: bool,
}

pub fn run(root: &Path) -> Result<(), Box<dyn Error>> {
    run_query(root, "")
}

pub fn run_query(root: &Path, query: &str) -> Result<(), Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other("browse needs an interactive terminal").into());
    }
    let mut app = App::new(root.canonicalize()?);
    app.query = query.to_owned();
    app.update_matches();
    let live = LiveIndex::start(&app.root)?;
    loop {
        let mut target = None;
        ratatui::run(|terminal| -> io::Result<()> {
            let mut redraw = true;
            loop {
                while let Some(update) = live.try_next()? {
                    app.apply_update(update);
                    redraw = true;
                }
                if redraw {
                    terminal.draw(|frame| app.draw(frame))?;
                    redraw = false;
                }
                if !event::poll(Duration::from_millis(50))? {
                    continue;
                }
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if key.code == KeyCode::Enter {
                            if let Some(symbol) = app.selected() {
                                target = Some((symbol.path.clone(), symbol.line, symbol.column));
                                return Ok(());
                            }
                        } else if key.code == KeyCode::F(5) {
                            live.request();
                            app.busy = true;
                        } else if app.handle_key(key) {
                            return Ok(());
                        }
                        redraw = true;
                    }
                    Event::Resize(_, _) => redraw = true,
                    _ => {}
                }
            }
        })?;
        let Some((path, line, column)) = target else {
            return Ok(());
        };
        // The terminal is restored before starting an interactive editor.
        match crate::editor::open(&path, line, column) {
            Ok(()) => {
                live.request();
                app.busy = true;
            }
            Err(error) => {
                app.status = format!("Editor: {error}");
            }
        }
    }
}

impl App {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            symbols: Vec::new(),
            matches: Vec::new(),
            query: String::new(),
            query_error: None,
            list: ListState::default(),
            preview: Ok(Vec::new()),
            offset: 0,
            status: String::new(),
            busy: true,
        }
    }

    fn apply_update(&mut self, update: Update) {
        match update {
            Update::Indexing => self.busy = true,
            Update::Failed(error) => {
                self.busy = false;
                self.status = format!("Index error: {error}");
            }
            Update::Ready(report, symbols) => {
                self.busy = false;
                self.replace_symbols(report, symbols);
            }
        }
    }

    fn replace_symbols(&mut self, report: index::IndexReport, symbols: Vec<Symbol>) {
        let previous = self.selected().map(|symbol| {
            (
                symbol.path.clone(),
                symbol.name.clone(),
                symbol.container.clone(),
                symbol.kind,
                symbol.line,
            )
        });
        let position = self.list.selected().unwrap_or(0);
        let offset = self.offset;
        self.symbols = symbols;
        self.update_matches();
        let retained = previous
            .as_ref()
            .and_then(|(path, name, container, kind, line)| {
                self.matches
                    .iter()
                    .enumerate()
                    .filter_map(|(position, &index)| {
                        let symbol = &self.symbols[index];
                        (symbol.path == *path
                            && symbol.name == *name
                            && symbol.kind == *kind
                            && symbol.container == *container)
                            .then_some((symbol.line.abs_diff(*line), position))
                    })
                    .min()
                    .map(|(_, position)| position)
            });
        self.list.select(
            retained.or_else(|| {
                (!self.matches.is_empty()).then(|| position.min(self.matches.len() - 1))
            }),
        );
        self.load_preview();
        if retained.is_some()
            && let Some((_, _, _, _, old_line)) = previous
            && let Some(symbol) = self.selected()
        {
            self.offset = if symbol.line >= old_line {
                offset.saturating_add(symbol.line - old_line)
            } else {
                offset.saturating_sub(old_line - symbol.line)
            };
            if let Ok(lines) = &self.preview {
                self.offset = self.offset.min(lines.len().saturating_sub(1));
            }
        }
        self.status = format!(
            "{} symbols | {} parsed | {} unchanged | {} removed",
            report.symbols_indexed,
            report.files_parsed,
            report.files_unchanged,
            report.files_removed
        );
    }

    fn update_matches(&mut self) {
        match code_scope::search::Query::parse(&self.query) {
            Ok(query) => {
                self.matches = query.rank_indices(&self.symbols, &self.root);
                self.query_error = None;
            }
            Err(error) => {
                self.matches.clear();
                self.query_error = Some(error);
            }
        }
    }

    fn filter(&mut self) {
        self.update_matches();
        self.list = ListState::default();
        self.list.select((!self.matches.is_empty()).then_some(0));
        self.load_preview();
    }

    fn selected(&self) -> Option<&Symbol> {
        let position = *self.matches.get(self.list.selected()?)?;
        self.symbols.get(position)
    }

    fn load_preview(&mut self) {
        let next = self
            .selected()
            .map(|symbol| (read_source(&symbol.path), symbol.line.saturating_sub(3)));
        let (preview, offset) = next.unwrap_or((Ok(Vec::new()), 0));
        self.preview = preview;
        self.offset = offset;
    }

    fn move_selection(&mut self, down: bool) {
        if let Some(position) = self.list.selected() {
            let next = if down {
                position
                    .saturating_add(1)
                    .min(self.matches.len().saturating_sub(1))
            } else {
                position.saturating_sub(1)
            };
            self.list.select(Some(next));
            self.load_preview();
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Char('c') if control => return true,
            KeyCode::Char('u') if control => {
                self.query.clear();
                self.filter();
            }
            KeyCode::Up => self.move_selection(false),
            KeyCode::Down => self.move_selection(true),
            KeyCode::PageUp => self.offset = self.offset.saturating_sub(10),
            KeyCode::PageDown => {
                let length = self.preview.as_ref().map_or(0, Vec::len);
                self.offset = self.offset.saturating_add(10).min(length.saturating_sub(1));
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.filter();
            }
            KeyCode::Char(character) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.query.push(character);
                self.filter();
            }
            _ => {}
        }
        false
    }

    fn draw(&mut self, frame: &mut Frame) {
        if frame.area().width < 72 || frame.area().height < 14 {
            frame.render_widget(
                Paragraph::new("Resize to at least 72 x 14. Esc quits."),
                frame.area(),
            );
            return;
        }
        let [header, search, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .areas(frame.area());
        frame.render_widget(
            Paragraph::new(format!(" SCOPE LIVE  {}", self.root.display())).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            header,
        );
        let query = Line::from(self.query.as_str());
        let width = query.width().min(u16::MAX as usize) as u16;
        let scroll = width.saturating_sub(search.width.saturating_sub(3));
        frame.render_widget(
            Paragraph::new(query)
                .scroll((0, scroll))
                .block(Block::bordered().title(" Fuzzy search | kind:method path:src ")),
            search,
        );
        frame.set_cursor_position((search.x + 1 + (width - scroll), search.y + 1));

        let [results, source] =
            Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                .areas(body);
        let items: Vec<_> = self
            .matches
            .iter()
            .map(|&position| {
                let symbol = &self.symbols[position];
                let path = symbol.path.strip_prefix(&self.root).unwrap_or(&symbol.path);
                ListItem::new(vec![
                    Line::from(format!("{}  {}", symbol.kind, symbol.display_name())),
                    Line::styled(
                        format!("{}:{}:{}", path.display(), symbol.line, symbol.column),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            })
            .collect();
        let title = format!(" Symbols {}/{} ", self.matches.len(), self.symbols.len());
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(if self.busy && self.symbols.is_empty() {
                    "Indexing repository..."
                } else {
                    "No matching symbols"
                })
                .block(Block::bordered().title(title)),
                results,
            );
        } else {
            frame.render_stateful_widget(
                List::new(items)
                    .block(Block::bordered().title(title))
                    .highlight_symbol("> ")
                    .highlight_style(
                        Style::default()
                            .bg(Color::DarkGray)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                results,
                &mut self.list,
            );
        }

        let selected = self.selected();
        let title = selected.map_or_else(
            || " Source ".to_owned(),
            |symbol| {
                let path = symbol.path.strip_prefix(&self.root).unwrap_or(&symbol.path);
                format!(" {}:{}:{} ", path.display(), symbol.line, symbol.column)
            },
        );
        let lines: Vec<Line<'_>> = match &self.preview {
            Ok(lines) => lines
                .iter()
                .enumerate()
                .skip(self.offset)
                .take(source.height.saturating_sub(2) as usize)
                .map(|(position, text)| {
                    let line = Line::from(vec![
                        Span::styled(
                            format!("{:>4} ", position + 1),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::raw(text.as_str()),
                    ]);
                    if selected.is_some_and(|symbol| symbol.line == position + 1) {
                        line.style(
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        line
                    }
                })
                .collect(),
            Err(error) => vec![Line::styled(
                error.as_str(),
                Style::default().fg(Color::Red),
            )],
        };
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(title)),
            source,
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(" Type: search  Enter: open  Up/Down: select  Ctrl+U: clear"),
                Line::from(" PgUp/PgDn: source  F5: refresh  Esc/Ctrl+C: quit"),
                Line::styled(
                    format!(
                        "{} | {}",
                        if self.busy { "Indexing..." } else { "Live" },
                        self.query_error.as_deref().unwrap_or(&self.status),
                    ),
                    Style::default().fg(Color::Cyan),
                ),
            ]),
            footer,
        );
    }
}

fn read_source(path: &Path) -> Result<Vec<String>, String> {
    const LIMIT: u64 = 2 * 1024 * 1024;
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(LIMIT + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("Cannot read source: {error}"))?;
    if bytes.len() as u64 > LIMIT {
        return Err("Source exceeds the 2 MiB preview limit".to_owned());
    }
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .map(|line| {
            line.replace('\t', "    ")
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect()
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_scope::SymbolKind;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn search_and_navigation_handle_an_empty_result_set() {
        let mut app = App::new(PathBuf::from("/repo"));
        app.symbols = ["Status", "set_status"]
            .into_iter()
            .map(|name| Symbol {
                container: None,
                name: name.to_owned(),
                kind: SymbolKind::Function,
                path: PathBuf::from("/repo/missing.rs"),
                line: 1,
                column: 1,
            })
            .collect();
        app.filter();
        for character in "STATUS".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(app.matches.len(), 2);
        app.move_selection(true);
        assert_eq!(app.selected().unwrap().name, "set_status");
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(
            app.selected().is_some(),
            "one extra query character should remain a fuzzy match"
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        app.move_selection(true);
        app.move_selection(false);
        assert!(app.selected().is_none());
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("No matching symbols"));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.selected().unwrap().name, "Status");
    }

    #[test]
    fn live_update_preserves_query_and_selection_when_lines_move() {
        let symbol = |name: &str, line| Symbol {
            container: None,
            name: name.to_owned(),
            kind: SymbolKind::Function,
            path: PathBuf::from("/repo/missing.rs"),
            line,
            column: 1,
        };
        let report = || index::IndexReport {
            files_parsed: 1,
            files_unchanged: 0,
            files_removed: 0,
            symbols_indexed: 2,
            database_path: PathBuf::new(),
        };
        let mut app = App::new(PathBuf::from("/repo"));
        app.query = "status".to_owned();
        app.apply_update(Update::Ready(
            report(),
            vec![symbol("Status", 1), symbol("set_status", 10)],
        ));
        app.move_selection(true);
        app.apply_update(Update::Ready(
            report(),
            vec![symbol("set_status", 14), symbol("Status", 20)],
        ));
        assert_eq!(app.query, "status");
        assert_eq!(app.selected().unwrap().name, "set_status");
        assert_eq!(app.selected().unwrap().line, 14);
        app.apply_update(Update::Ready(report(), Vec::new()));
        assert!(app.selected().is_none());
        app.move_selection(true);
    }
}

#[cfg(test)]
mod search_quality_tests {
    use super::*;

    #[test]
    fn browser_uses_shared_filters_and_shows_query_errors() {
        let mut app = App::new("/repo".into());
        app.symbols = vec![Symbol {
            name: "EngineTransaction".into(),
            kind: code_scope::SymbolKind::Method,
            path: "/repo/internal/engine.go".into(),
            line: 1,
            column: 1,
            container: Some("*Engine".into()),
        }];
        app.query = "engtxn kind:method path:internal".into();
        app.filter();
        assert_eq!(app.matches, [0]);
        assert_eq!(
            app.selected().unwrap().display_name(),
            "*Engine::EngineTransaction"
        );
        app.query = "kind:invalid".into();
        app.filter();
        assert!(app.matches.is_empty());
        assert!(app.query_error.is_some());
    }
}
