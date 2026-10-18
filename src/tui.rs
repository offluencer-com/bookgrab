//! Interactive terminal UI: search box, result list, cover preview and details.

use crate::abb::{AbbClient, BookDetails, SearchPage, SearchResult};
use crate::config::Config;
use crate::torrent;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use image::DynamicImage;
use ratatui::DefaultTerminal;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

const ACCENT: Color = Color::Cyan;
/// Covers are downscaled before encoding; terminal cells never need more than this.
const COVER_MAX_PX: u32 = 600;

/// Results of background work, delivered back to the UI thread.
enum Msg {
    Login(Result<()>),
    Search { query: String, page: u32, result: Result<SearchPage> },
    Details { url: String, result: Result<BookDetails> },
    Cover { url: String, result: Result<DynamicImage> },
    Sent { title: String, result: Result<String> },
}

#[derive(PartialEq)]
enum Focus {
    Search,
    List,
}

enum Cover {
    Loading,
    Ready(Box<StatefulProtocol>),
    Failed,
}

struct App {
    config: Config,
    client: Arc<AbbClient>,
    picker: Picker,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    focus: Focus,
    input: String,
    query: String,
    page: u32,
    has_next: bool,
    searching: bool,
    results: Vec<SearchResult>,
    list: ListState,
    details_scroll: u16,

    details: HashMap<String, Result<BookDetails, String>>,
    covers: HashMap<String, Cover>,
    /// Detail pages currently being fetched, keyed by book URL.
    pending_details: HashSet<String>,
    /// Books the user asked to download whose details are still loading.
    download_queue: HashSet<String>,

    status: Line<'static>,
    quit: bool,
}

pub fn run(config: Config, picker: Picker, initial_query: Option<String>) -> Result<()> {
    let client = Arc::new(AbbClient::new(&config.abb.base_url)?);
    let (tx, rx) = mpsc::channel();
    let mut app = App {
        config,
        client,
        picker,
        tx,
        rx,
        focus: Focus::Search,
        input: String::new(),
        query: String::new(),
        page: 1,
        has_next: false,
        searching: false,
        results: Vec::new(),
        list: ListState::default(),
        details_scroll: 0,
        details: HashMap::new(),
        covers: HashMap::new(),
        pending_details: HashSet::new(),
        download_queue: HashSet::new(),
        status: Line::from("Type a title or author and press Enter"),
        quit: false,
    };

    if !app.config.abb.username.is_empty() {
        let (client, cfg, tx) = (app.client.clone(), app.config.abb.clone(), app.tx.clone());
        app.status = Line::from("Logging in to AudiobookBay…");
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Login(client.login(&cfg.username, &cfg.password)));
        });
    }
    if let Some(q) = initial_query {
        app.input = q;
        app.submit_search();
    }

    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal);
    ratatui::restore();
    result
}

impl App {
    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.on_key(key);
                    }
                }
            }
            while let Ok(msg) = self.rx.try_recv() {
                self.on_msg(msg);
            }
        }
        Ok(())
    }

    // ---------- input ----------

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match self.focus {
            Focus::Search => match key.code {
                KeyCode::Enter => self.submit_search(),
                KeyCode::Esc if !self.results.is_empty() => self.focus = Focus::List,
                KeyCode::Esc => self.quit = true,
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(c) => self.input.push(c),
                KeyCode::Down | KeyCode::Tab if !self.results.is_empty() => self.focus = Focus::List,
                _ => {}
            },
            Focus::List => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                KeyCode::Char('/') | KeyCode::Char('s') | KeyCode::Tab => self.focus = Focus::Search,
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::PageDown => self.move_selection(10),
                KeyCode::PageUp => self.move_selection(-10),
                KeyCode::Home | KeyCode::Char('g') => self.select(0),
                KeyCode::End | KeyCode::Char('G') => self.select(self.results.len().saturating_sub(1)),
                KeyCode::Right | KeyCode::Char('n') | KeyCode::Char('l') if self.has_next => {
                    self.load_page(self.page + 1)
                }
                KeyCode::Left | KeyCode::Char('p') | KeyCode::Char('h') if self.page > 1 => {
                    self.load_page(self.page - 1)
                }
                KeyCode::Char('J') => self.details_scroll = self.details_scroll.saturating_add(3),
                KeyCode::Char('K') => self.details_scroll = self.details_scroll.saturating_sub(3),
                KeyCode::Enter | KeyCode::Char('d') => self.download_selected(),
                KeyCode::Char('o') => self.open_in_browser(),
                _ => {}
            },
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            return;
        }
        let current = self.list.selected().unwrap_or(0) as isize;
        let last = self.results.len() as isize - 1;
        self.select((current + delta).clamp(0, last) as usize);
    }

    fn select(&mut self, index: usize) {
        if self.results.is_empty() {
            return;
        }
        self.list.select(Some(index));
        self.details_scroll = 0;
        self.load_selected();
    }

    fn selected(&self) -> Option<&SearchResult> {
        self.list.selected().and_then(|i| self.results.get(i))
    }

    // ---------- background work ----------

    fn spawn(&self, job: impl FnOnce(&AbbClient) -> Msg + Send + 'static) {
        let (client, tx) = (self.client.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(job(&client));
        });
    }

    fn submit_search(&mut self) {
        let query = self.input.trim().to_string();
        if query.is_empty() {
            return;
        }
        self.query = query;
        self.load_page(1);
    }

    fn load_page(&mut self, page: u32) {
        self.searching = true;
        self.status = Line::from(format!("Searching “{}” (page {page})…", self.query));
        let query = self.query.clone();
        self.spawn(move |c| Msg::Search { result: c.search(&query, page), query, page });
    }

    /// Starts fetching the cover and details of the selected book, unless already cached.
    fn load_selected(&mut self) {
        let Some(book) = self.selected().cloned() else { return };

        if let Some(cover_url) = book.cover_url.clone() {
            if !self.covers.contains_key(&cover_url) {
                self.covers.insert(cover_url.clone(), Cover::Loading);
                self.spawn(move |c| Msg::Cover { result: fetch_cover(c, &cover_url), url: cover_url });
            }
        }
        self.load_details(&book.url);
    }

    fn load_details(&mut self, url: &str) {
        if self.details.contains_key(url) || !self.pending_details.insert(url.to_string()) {
            return;
        }
        let url = url.to_string();
        self.spawn(move |c| Msg::Details { result: c.details(&url), url });
    }

    fn download_selected(&mut self) {
        let Some(book) = self.selected().cloned() else { return };
        match self.details.get(&book.url) {
            Some(Ok(details)) => self.send_magnet(details.clone()),
            Some(Err(e)) => self.status = error_line(format!("Cannot download: {e}")),
            None => {
                self.status = Line::from(format!("Fetching magnet for “{}”…", book.title));
                self.download_queue.insert(book.url.clone());
                self.load_details(&book.url);
            }
        }
    }

    fn send_magnet(&mut self, details: BookDetails) {
        self.status = Line::from(format!("Sending “{}” to qBittorrent…", details.title));
        let cfg = self.config.qbittorrent.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = torrent::add_magnet(&cfg, &details.magnet());
            let _ = tx.send(Msg::Sent { title: details.title, result });
        });
    }

    fn open_in_browser(&mut self) {
        let Some(book) = self.selected() else { return };
        let opened = std::process::Command::new("xdg-open")
            .arg(&book.url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Err(e) = opened {
            self.status = error_line(format!("Could not open browser: {e}"));
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Login(Ok(())) => self.status = Line::from("Logged in to AudiobookBay"),
            Msg::Login(Err(e)) => self.status = error_line(format!("{e:#}")),
            Msg::Search { query, page, result } => {
                // Ignore responses for searches the user has since replaced.
                if query != self.query {
                    return;
                }
                self.searching = false;
                match result {
                    Ok(found) => {
                        self.page = page;
                        self.has_next = found.has_next;
                        self.results = found.results;
                        self.status = if self.results.is_empty() {
                            Line::from(format!("No results for “{query}”"))
                        } else {
                            Line::from(format!("{} results on page {page}", self.results.len()))
                        };
                        if !self.results.is_empty() {
                            self.focus = Focus::List;
                            self.select(0);
                        }
                    }
                    Err(e) => self.status = error_line(format!("Search failed: {e:#}")),
                }
            }
            Msg::Details { url, result } => {
                self.pending_details.remove(&url);
                let result = result.map_err(|e| format!("{e:#}"));
                if self.download_queue.remove(&url) {
                    match &result {
                        Ok(details) => self.send_magnet(details.clone()),
                        Err(e) => self.status = error_line(format!("Cannot download: {e}")),
                    }
                }
                self.details.insert(url, result);
            }
            Msg::Cover { url, result } => {
                let cover = match result {
                    Ok(img) => Cover::Ready(Box::new(self.picker.new_resize_protocol(img))),
                    Err(_) => Cover::Failed,
                };
                self.covers.insert(url, cover);
            }
            Msg::Sent { title, result } => {
                self.status = match result {
                    Ok(via) => Line::from(vec![
                        Span::styled("✓ ", Style::new().fg(Color::Green).bold()),
                        Span::raw(format!("“{title}” added via {via} → {}", self.config.qbittorrent.save_path)),
                    ]),
                    Err(e) => error_line(format!("qBittorrent: {e:#}")),
                }
            }
        }
    }

    // ---------- drawing ----------

    fn draw(&mut self, f: &mut Frame) {
        let [search_area, main_area, status_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(5), Constraint::Length(2)]).areas(f.area());
        self.draw_search(f, search_area);

        let [list_area, side_area] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(main_area);
        self.draw_list(f, list_area);
        self.draw_side(f, side_area);
        self.draw_status(f, status_area);
    }

    fn draw_search(&self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Search;
        let block = panel(" bookgrab · search AudiobookBay ", focused);
        let mut spans = vec![Span::styled("❯ ", Style::new().fg(ACCENT)), Span::raw(self.input.clone())];
        if focused {
            spans.push(Span::styled("█", Style::new().fg(ACCENT).add_modifier(Modifier::SLOW_BLINK)));
        }
        if self.searching {
            spans.push(Span::styled("  searching…", Style::new().dim()));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let mut title = format!(" Results · page {} ", self.page);
        if self.has_next {
            title.push_str("→ ");
        }
        let block = panel(&title, self.focus == Focus::List);
        let width = area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .results
            .iter()
            .map(|r| {
                let meta = [r.format.as_str(), r.size.as_str(), r.language.as_str(), r.posted.as_str()]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" · ");
                ListItem::new(vec![
                    Line::from(truncate(&r.title, width)).bold(),
                    Line::from(truncate(&meta, width)).dim(),
                ])
            })
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().bg(Color::Rgb(40, 60, 70)))
            .highlight_symbol("▌");
        f.render_stateful_widget(list, area, &mut self.list);
    }

    fn draw_side(&mut self, f: &mut Frame, area: Rect) {
        let Some(book) = self.selected().cloned() else {
            f.render_widget(panel(" Details ", false), area);
            return;
        };
        let [cover_area, info_area] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

        let cover_block = panel(" Cover ", false);
        let inner = cover_block.inner(cover_area);
        f.render_widget(cover_block, cover_area);
        match book.cover_url.as_ref().and_then(|u| self.covers.get_mut(u)) {
            Some(Cover::Ready(protocol)) => {
                f.render_stateful_widget(StatefulImage::default(), centered_cover(inner), protocol.as_mut())
            }
            Some(Cover::Loading) => f.render_widget(Paragraph::new("loading cover…").dim(), inner),
            _ => f.render_widget(Paragraph::new("no cover").dim(), inner),
        }

        let mut lines: Vec<Line> = vec![Line::from(book.title.clone()).bold().fg(ACCENT), Line::default()];
        let field = |label: &str, value: String| {
            Line::from(vec![Span::styled(format!("{label:<10}"), Style::new().dim()), Span::raw(value)])
        };
        match self.details.get(&book.url) {
            Some(Ok(d)) => {
                if !d.authors.is_empty() {
                    lines.push(field("Author", d.authors.join(", ")));
                }
                if !d.narrators.is_empty() {
                    lines.push(field("Narrator", d.narrators.join(", ")));
                }
                lines.push(field("Format", format!("{} · {}", d.format, book.bitrate)));
                let size = if d.size.is_empty() { &book.size } else { &d.size };
                lines.push(field("Size", size.clone()));
                lines.push(field("Category", book.category.clone()));
                lines.push(field("Language", book.language.clone()));
                lines.push(Line::default());
                lines.extend(d.description.lines().map(|l| Line::from(l.to_string())));
            }
            Some(Err(e)) => lines.push(Line::from(format!("Could not load details: {e}")).fg(Color::Red)),
            None => {
                lines.push(field("Format", format!("{} · {}", book.format, book.bitrate)));
                lines.push(field("Size", book.size.clone()));
                lines.push(field("Category", book.category.clone()));
                lines.push(field("Language", book.language.clone()));
                lines.push(Line::default());
                lines.push(Line::from("loading details…").dim());
            }
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .scroll((self.details_scroll, 0))
                .block(panel(" Details · J/K scroll ", false)),
            info_area,
        );
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let keys = match self.focus {
            Focus::Search => "enter search · esc results/quit · tab results",
            Focus::List => "↑↓/jk move · enter/d download · ←→/hn page · o open in browser · / search · q quit",
        };
        let text = vec![self.status.clone(), Line::from(keys).dim()];
        f.render_widget(Paragraph::new(text), area);
    }
}

fn fetch_cover(client: &AbbClient, url: &str) -> Result<DynamicImage> {
    let bytes = client.fetch_bytes(url)?;
    let img = image::load_from_memory(&bytes)?;
    Ok(if img.width() > COVER_MAX_PX || img.height() > COVER_MAX_PX {
        img.thumbnail(COVER_MAX_PX, COVER_MAX_PX)
    } else {
        img
    })
}

fn panel(title: &str, focused: bool) -> Block<'static> {
    let color = if focused { ACCENT } else { Color::DarkGray };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(title.to_string())
}

/// Leaves a one-cell margin so the image doesn't touch the border.
fn centered_cover(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    }
}

fn error_line(msg: String) -> Line<'static> {
    Line::from(vec![Span::styled("✗ ", Style::new().fg(Color::Red).bold()), Span::raw(msg)])
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
