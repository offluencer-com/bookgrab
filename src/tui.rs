//! Interactive terminal UI: search kind tabs, result list, cover preview and details.

use crate::abb::{AbbClient, BookDetails, SearchResult};
use crate::config::{Config, expand_home};
use crate::libgen::{Ebook, LibgenClient};
use crate::torrent;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use image::DynamicImage;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph, Wrap};
use ratatui_image::StatefulImage;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// Covers are downscaled before encoding; terminal cells never need more than this.
const COVER_MAX_PX: u32 = 600;
/// A cover that failed to load is fetched again when selected after this long.
const COVER_RETRY_AFTER: Duration = Duration::from_secs(30);
/// Neither site can filter by file type or language, so results are filtered here and
/// pages without a single match are skipped, up to this many pages per search.
const MAX_PAGES_PER_SEARCH: u32 = 3;
/// Language filter value that shows every language.
const ALL_LANGUAGES: &str = "All";

/// What the user is searching for. Each kind has its own source and colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Audiobook,
    Epub,
    Pdf,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::Audiobook, Kind::Epub, Kind::Pdf];

    fn label(self) -> &'static str {
        match self {
            Kind::Audiobook => "♫ Audiobook",
            Kind::Epub => "◧ EPUB",
            Kind::Pdf => "▤ PDF",
        }
    }

    fn color(self) -> Color {
        match self {
            Kind::Audiobook => Color::Cyan,
            Kind::Epub => Color::Green,
            Kind::Pdf => Color::LightRed,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Kind::Audiobook => "",
            Kind::Epub => "epub",
            Kind::Pdf => "pdf",
        }
    }

    fn cycle(self, step: isize) -> Kind {
        let i = Kind::ALL.iter().position(|k| *k == self).unwrap() as isize;
        Kind::ALL[(i + step).rem_euclid(Kind::ALL.len() as isize) as usize]
    }
}

struct Sources {
    abb: AbbClient,
    libgen: LibgenClient,
}

impl Sources {
    /// Fetches pages starting at `page` until one has results of the right type and language.
    fn search(&self, kind: Kind, query: &str, language: &str, page: u32) -> Result<Results> {
        let mut page = page.max(1);
        let last_try = page + MAX_PAGES_PER_SEARCH - 1;
        loop {
            let mut found = self.search_page(kind, query, page)?;
            found.items.retain(|item| item.matches(kind, language));
            if !found.items.is_empty() || !found.has_next || page >= last_try {
                return Ok(found);
            }
            page += 1;
        }
    }

    fn search_page(&self, kind: Kind, query: &str, page: u32) -> Result<Results> {
        Ok(match kind {
            Kind::Audiobook => {
                let r = self.abb.search(query, page)?;
                Results { items: r.results.into_iter().map(Item::Audio).collect(), page, has_next: r.has_next }
            }
            Kind::Epub | Kind::Pdf => {
                let r = self.libgen.search(query, page)?;
                Results {
                    items: r.results.into_iter().map(Item::Ebook).collect(),
                    page,
                    has_next: page < r.total_pages,
                }
            }
        })
    }
}

#[derive(Clone)]
enum Item {
    Audio(SearchResult),
    Ebook(Ebook),
}

impl Item {
    /// Stable identifier used for caches: book page URL or file md5.
    fn key(&self) -> &str {
        match self {
            Item::Audio(r) => &r.url,
            Item::Ebook(b) => &b.md5,
        }
    }

    fn title(&self) -> &str {
        match self {
            Item::Audio(r) => &r.title,
            Item::Ebook(b) => &b.title,
        }
    }

    fn matches(&self, kind: Kind, language: &str) -> bool {
        let (item_language, type_ok) = match self {
            Item::Audio(r) => (&r.language, true),
            Item::Ebook(b) => (&b.language, b.extension.eq_ignore_ascii_case(kind.extension())),
        };
        // Some books list several languages ("English; Russian").
        type_ok
            && (language == ALL_LANGUAGES || item_language.to_lowercase().contains(&language.to_lowercase()))
    }

    fn cover_url(&self) -> Option<&String> {
        match self {
            Item::Audio(r) => r.cover_url.as_ref(),
            Item::Ebook(b) => b.cover_url.as_ref(),
        }
    }

    fn meta(&self) -> String {
        let parts: Vec<&str> = match self {
            Item::Audio(r) => vec![&r.format, &r.size, &r.language, &r.posted],
            Item::Ebook(b) => vec![&b.authors, &b.year, &b.size, &b.language],
        };
        parts.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ")
    }
}

struct Results {
    items: Vec<Item>,
    page: u32,
    has_next: bool,
}

/// Results of background work, delivered back to the UI thread.
enum Msg {
    Login(Result<()>),
    Search { seq: u64, result: Result<Results> },
    Details { url: String, result: Result<BookDetails> },
    Cover { url: String, result: Result<DynamicImage> },
    Sent { key: String, title: String, result: Result<String> },
    Progress { title: String, downloaded: u64, total: Option<u64> },
    Saved { key: String, title: String, result: Result<PathBuf> },
}

#[derive(PartialEq)]
enum Focus {
    Search,
    List,
}

enum Cover {
    Loading,
    Ready(Box<StatefulProtocol>),
    Failed(Instant),
}

struct App {
    config: Config,
    sources: Arc<Sources>,
    picker: Picker,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    kind: Kind,
    language: String,
    focus: Focus,
    input: String,
    query: String,
    /// Incremented per search so late responses from older searches are ignored.
    search_seq: u64,
    page: u32,
    has_next: bool,
    searching: bool,
    results: Vec<Item>,
    list: ListState,
    details_scroll: u16,

    details: HashMap<String, Result<BookDetails, String>>,
    covers: HashMap<String, Cover>,
    /// Audiobook detail pages currently being fetched, keyed by book URL.
    pending_details: HashSet<String>,
    /// Audiobooks the user asked to download whose details are still loading.
    download_queue: HashSet<String>,
    /// Ebooks being downloaded, keyed by md5.
    downloading: HashSet<String>,
    /// Items already sent to qBittorrent or saved, marked ✓ in the list.
    done: HashSet<String>,

    status: Line<'static>,
    quit: bool,
}

pub fn run(config: Config, picker: Picker, kind: Kind, initial_query: Option<String>) -> Result<()> {
    let sources = Arc::new(Sources {
        abb: AbbClient::new(&config.abb.base_url)?,
        libgen: LibgenClient::new(&config.libgen.base_url)?,
    });
    let (tx, rx) = mpsc::channel();
    let language = config.search.language.clone();
    let mut app = App {
        config,
        sources,
        picker,
        tx,
        rx,
        language,
        kind,
        focus: Focus::Search,
        input: String::new(),
        query: String::new(),
        search_seq: 0,
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
        downloading: HashSet::new(),
        done: HashSet::new(),
        status: Line::from("Pick what to search for with Tab, type a title or author and press Enter"),
        quit: false,
    };

    if !app.config.abb.username.is_empty() {
        let cfg = app.config.abb.clone();
        app.status = Line::from("Logging in to AudiobookBay…");
        app.spawn(move |s| Msg::Login(s.abb.login(&cfg.username, &cfg.password)));
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
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => self.quit = true,
                KeyCode::Char('l') => self.cycle_language(),
                _ => {}
            }
            return;
        }
        match self.focus {
            Focus::Search => match key.code {
                KeyCode::Enter => self.submit_search(),
                KeyCode::Tab => self.set_kind(self.kind.cycle(1)),
                KeyCode::BackTab => self.set_kind(self.kind.cycle(-1)),
                KeyCode::Esc | KeyCode::Down if !self.results.is_empty() => self.focus = Focus::List,
                KeyCode::Esc => self.quit = true,
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(c) => self.input.push(c),
                _ => {}
            },
            Focus::List => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                KeyCode::Char('/') | KeyCode::Char('s') | KeyCode::Tab => self.focus = Focus::Search,
                KeyCode::Char('1') => self.set_kind(Kind::Audiobook),
                KeyCode::Char('2') => self.set_kind(Kind::Epub),
                KeyCode::Char('3') => self.set_kind(Kind::Pdf),
                KeyCode::Char('L') => self.cycle_language(),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::PageDown => self.move_selection(10),
                KeyCode::PageUp => self.move_selection(-10),
                KeyCode::Home | KeyCode::Char('g') => self.select(0),
                KeyCode::End | KeyCode::Char('G') => self.select(self.results.len().saturating_sub(1)),
                KeyCode::Char('J') => self.details_scroll = self.details_scroll.saturating_add(3),
                KeyCode::Char('K') => self.details_scroll = self.details_scroll.saturating_sub(3),
                KeyCode::Right | KeyCode::Char('n') | KeyCode::Char('l') if self.has_next => {
                    self.load_page(self.page + 1)
                }
                KeyCode::Left | KeyCode::Char('p') | KeyCode::Char('h') if self.page > 1 => {
                    self.load_page(self.page - 1)
                }
                KeyCode::Enter | KeyCode::Char('d') => self.download_selected(),
                KeyCode::Char('o') => self.open_in_browser(),
                _ => {}
            },
        }
    }

    /// Switches what we search for; repeats the current search right away if there is one.
    fn set_kind(&mut self, kind: Kind) {
        if kind == self.kind {
            return;
        }
        self.kind = kind;
        self.results.clear();
        self.list.select(None);
        self.page = 1;
        self.has_next = false;
        self.search_seq += 1;
        self.searching = false;
        self.focus = Focus::Search;
        if self.input.trim().is_empty() {
            self.status = Line::from(format!("Searching for {} now; type a title or author", self.kind_name()));
        } else {
            self.submit_search();
        }
    }

    /// Steps through "All" and the configured languages; repeats the search if there is one.
    fn cycle_language(&mut self) {
        let mut options = vec![ALL_LANGUAGES.to_string()];
        options.extend(self.config.search.languages.iter().cloned());
        let current = options.iter().position(|l| l.eq_ignore_ascii_case(&self.language));
        self.language = match current {
            Some(i) => options[(i + 1) % options.len()].clone(),
            None => options[0].clone(),
        };
        if self.query.is_empty() {
            self.status = Line::from(format!("Language filter: {}", self.language));
        } else {
            self.load_page(1);
        }
    }

    /// " in English", or nothing when every language is shown.
    fn language_suffix(&self) -> String {
        if self.language == ALL_LANGUAGES { String::new() } else { format!(" in {}", self.language) }
    }

    fn kind_name(&self) -> &'static str {
        match self.kind {
            Kind::Audiobook => "audiobooks",
            Kind::Epub => "EPUB ebooks",
            Kind::Pdf => "PDF ebooks",
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

    fn selected(&self) -> Option<&Item> {
        self.list.selected().and_then(|i| self.results.get(i))
    }

    // ---------- background work ----------

    fn spawn(&self, job: impl FnOnce(&Sources) -> Msg + Send + 'static) {
        let (sources, tx) = (self.sources.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(job(&sources));
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
        self.search_seq += 1;
        self.searching = true;
        self.status = Line::from(format!(
            "Searching {}{} for “{}” (page {page})…",
            self.kind_name(),
            self.language_suffix(),
            self.query
        ));
        let (seq, query, kind, language) = (self.search_seq, self.query.clone(), self.kind, self.language.clone());
        self.spawn(move |s| Msg::Search { seq, result: s.search(kind, &query, &language, page) });
    }

    /// Starts fetching the cover (and audiobook details) of the selected item, unless cached.
    fn load_selected(&mut self) {
        let Some(item) = self.selected().cloned() else { return };

        if let Some(cover_url) = item.cover_url().cloned() {
            let retry = match self.covers.get(&cover_url) {
                None => true,
                Some(Cover::Failed(at)) => at.elapsed() >= COVER_RETRY_AFTER,
                Some(_) => false,
            };
            if retry {
                self.covers.insert(cover_url.clone(), Cover::Loading);
                let from_libgen = matches!(item, Item::Ebook(_));
                self.spawn(move |s| {
                    let bytes = if from_libgen { s.libgen.fetch_cover(&cover_url) } else { s.abb.fetch_bytes(&cover_url) };
                    Msg::Cover { result: bytes.and_then(|b| decode_cover(&b)), url: cover_url }
                });
            }
        }
        if let Item::Audio(book) = item {
            self.load_details(&book.url);
        }
    }

    fn load_details(&mut self, url: &str) {
        if self.details.contains_key(url) || !self.pending_details.insert(url.to_string()) {
            return;
        }
        let url = url.to_string();
        self.spawn(move |s| Msg::Details { result: s.abb.details(&url), url });
    }

    fn download_selected(&mut self) {
        match self.selected().cloned() {
            Some(Item::Audio(book)) => match self.details.get(&book.url) {
                Some(Ok(details)) => self.send_magnet(book.url.clone(), details.clone()),
                Some(Err(e)) => self.status = error_line(format!("Cannot download: {e}")),
                None => {
                    self.status = Line::from(format!("Fetching magnet for “{}”…", book.title));
                    self.download_queue.insert(book.url.clone());
                    self.load_details(&book.url);
                }
            },
            Some(Item::Ebook(book)) => self.download_ebook(book),
            None => {}
        }
    }

    fn send_magnet(&mut self, key: String, details: BookDetails) {
        self.status = Line::from(format!("Sending “{}” to qBittorrent…", details.title));
        let cfg = self.config.qbittorrent.clone();
        self.spawn(move |_| {
            let result = torrent::add_magnet(&cfg, &details.magnet());
            Msg::Sent { key, title: details.title, result }
        });
    }

    fn download_ebook(&mut self, book: Ebook) {
        if !self.downloading.insert(book.md5.clone()) {
            self.status = Line::from(format!("“{}” is already downloading", book.title));
            return;
        }
        self.status = Line::from(format!("Downloading “{}”…", book.title));
        let dir = expand_home(&self.config.libgen.save_path);
        let (sources, tx) = (self.sources.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let mut last_report = Instant::now();
            let result = sources.libgen.download(&book, &dir, |downloaded, total| {
                if last_report.elapsed() >= Duration::from_millis(200) {
                    last_report = Instant::now();
                    let _ = tx.send(Msg::Progress { title: book.title.clone(), downloaded, total });
                }
            });
            let _ = tx.send(Msg::Saved { key: book.md5.clone(), title: book.title.clone(), result });
        });
    }

    fn open_in_browser(&mut self) {
        let url = match self.selected() {
            Some(Item::Audio(r)) => r.url.clone(),
            Some(Item::Ebook(b)) => format!("{}/ads.php?md5={}", self.config.libgen.base_url, b.md5),
            None => return,
        };
        let opened = std::process::Command::new("xdg-open")
            .arg(&url)
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
            Msg::Search { seq, result } => {
                if seq != self.search_seq {
                    return;
                }
                self.searching = false;
                match result {
                    Ok(found) => {
                        self.page = found.page;
                        self.has_next = found.has_next;
                        self.results = found.items;
                        self.status = if self.results.is_empty() {
                            Line::from(format!(
                                "No {}{} found for “{}” (L changes the language, → tries the next page)",
                                self.kind_name(),
                                self.language_suffix(),
                                self.query
                            ))
                        } else {
                            Line::from(format!("{} results on page {}", self.results.len(), self.page))
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
                        Ok(details) => self.send_magnet(url.clone(), details.clone()),
                        Err(e) => self.status = error_line(format!("Cannot download: {e}")),
                    }
                }
                self.details.insert(url, result);
            }
            Msg::Cover { url, result } => {
                let cover = match result {
                    Ok(img) => Cover::Ready(Box::new(self.picker.new_resize_protocol(img))),
                    Err(_) => Cover::Failed(Instant::now()),
                };
                self.covers.insert(url, cover);
            }
            Msg::Sent { key, title, result } => {
                self.status = match result {
                    Ok(via) => {
                        self.done.insert(key);
                        ok_line(format!("“{title}” added via {via} → {}", self.config.qbittorrent.save_path))
                    }
                    Err(e) => error_line(format!("qBittorrent: {e:#}")),
                }
            }
            Msg::Progress { title, downloaded, total } => {
                let amount = match total {
                    Some(t) if t > 0 => format!("{}% ({} / {})", downloaded * 100 / t, mb(downloaded), mb(t)),
                    _ => mb(downloaded),
                };
                self.status = Line::from(format!("↓ Downloading “{title}” {amount}"));
            }
            Msg::Saved { key, title, result } => {
                self.downloading.remove(&key);
                self.status = match result {
                    Ok(path) => {
                        self.done.insert(key);
                        ok_line(format!("“{title}” saved to {}", path.display()))
                    }
                    Err(e) => error_line(format!("Download failed: {e:#}")),
                }
            }
        }
    }

    // ---------- drawing ----------

    fn draw(&mut self, f: &mut Frame) {
        let [search_area, main_area, status_area] =
            Layout::vertical([Constraint::Length(4), Constraint::Min(5), Constraint::Length(2)]).areas(f.area());
        self.draw_search(f, search_area);

        let [list_area, side_area] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(main_area);
        self.draw_list(f, list_area);
        self.draw_side(f, side_area);
        self.draw_status(f, status_area);
    }

    fn draw_search(&self, f: &mut Frame, area: Rect) {
        let color = self.kind.color();
        let focused = self.focus == Focus::Search;

        let mut tabs = Vec::new();
        for kind in Kind::ALL {
            let label = format!(" {} ", kind.label());
            tabs.push(if kind == self.kind {
                Span::styled(label, Style::new().bg(kind.color()).fg(Color::Black).bold())
            } else {
                Span::styled(label, Style::new().fg(kind.color()).dim())
            });
            tabs.push(Span::raw(" "));
        }
        tabs.push(Span::styled("│ Language: ", Style::new().dim()));
        tabs.push(Span::styled(self.language.clone(), Style::new().fg(color).bold()));
        tabs.push(Span::styled(" (ctrl+l) ", Style::new().dim()));
        let source = match self.kind {
            Kind::Audiobook => format!("AudiobookBay → qBittorrent → {}", self.config.qbittorrent.save_path),
            Kind::Epub | Kind::Pdf => format!("LibGen → {}", self.config.libgen.save_path),
        };
        tabs.push(Span::styled(format!(" {source}"), Style::new().dim()));

        let mut input = vec![Span::styled("❯ ", Style::new().fg(color)), Span::raw(self.input.clone())];
        if focused {
            input.push(Span::styled("█", Style::new().fg(color).add_modifier(Modifier::SLOW_BLINK)));
        }
        if self.searching {
            input.push(Span::styled("  searching…", Style::new().dim()));
        }

        let block = panel(" bookgrab · Tab switches type ", focused, color);
        f.render_widget(Paragraph::new(vec![Line::from(tabs), Line::from(input)]).block(block), area);
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let mut title = format!(" {} · {} · page {} ", self.kind.label(), self.language, self.page);
        if self.has_next {
            title.push_str("→ ");
        }
        let block = panel(&title, self.focus == Focus::List, self.kind.color());
        if self.results.is_empty() {
            f.render_widget(Paragraph::new(self.welcome()).wrap(Wrap { trim: false }).block(block), area);
            return;
        }
        let width = area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .results
            .iter()
            .map(|item| {
                let mark = if self.done.contains(item.key()) {
                    "✓ "
                } else if self.downloading.contains(item.key()) {
                    "↓ "
                } else {
                    ""
                };
                ListItem::new(vec![
                    Line::from(truncate(&format!("{mark}{}", item.title()), width)).bold(),
                    Line::from(truncate(&item.meta(), width)).dim(),
                ])
            })
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().bg(Color::Rgb(40, 60, 70)))
            .highlight_symbol("▌");
        f.render_stateful_widget(list, area, &mut self.list);
    }

    /// Shown instead of an empty list: explains the three search types.
    fn welcome(&self) -> Vec<Line<'static>> {
        let row = |kind: Kind, text: &str| {
            let marker = if kind == self.kind { "▸ " } else { "  " };
            Line::from(vec![
                Span::styled(format!("{marker}{:<13}", kind.label()), Style::new().fg(kind.color()).bold()),
                Span::raw(text.to_string()),
            ])
        };
        vec![
            Line::default(),
            Line::from("  What are you looking for?").bold(),
            Line::default(),
            row(Kind::Audiobook, "AudiobookBay → qBittorrent"),
            row(Kind::Epub, "LibGen, for e-readers & phones"),
            row(Kind::Pdf, "LibGen, books & documents"),
            Line::default(),
            Line::from("  Tab / Shift+Tab switches the type, Enter searches.").dim(),
            Line::from("  Ctrl+L (or L in the list) changes the language.").dim(),
        ]
    }

    fn draw_side(&mut self, f: &mut Frame, area: Rect) {
        let Some(item) = self.selected().cloned() else {
            f.render_widget(panel(" Details ", false, Color::DarkGray), area);
            return;
        };
        let [cover_area, info_area] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

        let cover_block = panel(" Cover ", false, Color::DarkGray);
        let inner = cover_block.inner(cover_area);
        f.render_widget(cover_block, cover_area);
        match item.cover_url().and_then(|u| self.covers.get_mut(u)) {
            Some(Cover::Ready(protocol)) => {
                f.render_stateful_widget(StatefulImage::default(), centered_cover(inner), protocol.as_mut())
            }
            Some(Cover::Loading) => f.render_widget(Paragraph::new("loading cover…").dim(), inner),
            _ => f.render_widget(Paragraph::new("no cover").dim(), inner),
        }

        let lines = match &item {
            Item::Audio(book) => self.audio_details(book),
            Item::Ebook(book) => self.ebook_details(book),
        };
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .scroll((self.details_scroll, 0))
                .block(panel(" Details · J/K scroll ", false, Color::DarkGray)),
            info_area,
        );
    }

    fn audio_details(&self, book: &SearchResult) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(book.title.clone()).bold().fg(self.kind.color()), Line::default()];
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
        lines
    }

    fn ebook_details(&self, book: &Ebook) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(book.title.clone()).bold().fg(self.kind.color()), Line::default()];
        let fields = [
            ("Author", &book.authors),
            ("Series", &book.series),
            ("Publisher", &book.publisher),
            ("Year", &book.year),
            ("Language", &book.language),
            ("Pages", &book.pages),
            ("Size", &book.size),
        ];
        for (label, value) in fields {
            if !value.is_empty() {
                lines.push(field(label, value.clone()));
            }
        }
        lines.push(field("Format", book.extension.to_uppercase()));
        lines.push(Line::default());
        lines.push(Line::from(format!("Enter saves it to {}", self.config.libgen.save_path)).dim());
        lines
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let keys = match self.focus {
            Focus::Search => "enter search · tab/shift+tab type · ctrl+l language · ↓/esc results · esc quit",
            Focus::List => {
                "↑↓/jk move · enter download · ←→ page · 1/2/3 type · L language · o browser · / search · q quit"
            }
        };
        let text = vec![self.status.clone(), Line::from(keys).dim()];
        f.render_widget(Paragraph::new(text), area);
    }
}

fn decode_cover(bytes: &[u8]) -> Result<DynamicImage> {
    let img = image::load_from_memory(bytes)?;
    Ok(if img.width() > COVER_MAX_PX || img.height() > COVER_MAX_PX {
        img.thumbnail(COVER_MAX_PX, COVER_MAX_PX)
    } else {
        img
    })
}

fn panel(title: &str, focused: bool, color: Color) -> Block<'static> {
    let color = if focused { color } else { Color::DarkGray };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(title.to_string())
}

fn field(label: &str, value: String) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{label:<10}"), Style::new().dim()), Span::raw(value)])
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

fn ok_line(msg: String) -> Line<'static> {
    Line::from(vec![Span::styled("✓ ", Style::new().fg(Color::Green).bold()), Span::raw(msg)])
}

fn error_line(msg: String) -> Line<'static> {
    Line::from(vec![Span::styled("✗ ", Style::new().fg(Color::Red).bold()), Span::raw(msg)])
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ebook(extension: &str, language: &str) -> Item {
        Item::Ebook(Ebook { extension: extension.into(), language: language.into(), ..Default::default() })
    }

    #[test]
    fn filters_by_type_and_language() {
        assert!(ebook("epub", "English").matches(Kind::Epub, "English"));
        assert!(ebook("epub", "English").matches(Kind::Epub, ALL_LANGUAGES));
        assert!(!ebook("epub", "English").matches(Kind::Pdf, ALL_LANGUAGES));
        assert!(!ebook("epub", "French").matches(Kind::Epub, "English"));
        assert!(ebook("pdf", "English; Russian").matches(Kind::Pdf, "russian"));

        let audio = Item::Audio(SearchResult { language: "Spanish".into(), ..Default::default() });
        assert!(audio.matches(Kind::Audiobook, "Spanish"));
        assert!(!audio.matches(Kind::Audiobook, "English"));
    }

    /// Hits the real sites; run with `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_language_filter() {
        let config = Config::default();
        let sources = Sources {
            abb: AbbClient::new(&config.abb.base_url).unwrap(),
            libgen: LibgenClient::new(&config.libgen.base_url).unwrap(),
        };
        for kind in [Kind::Audiobook, Kind::Epub] {
            let found = sources.search(kind, "hobbit", "English", 1).unwrap();
            println!("{kind:?}: page {}, {} results", found.page, found.items.len());
            for item in found.items.iter().take(4) {
                println!("  {} | {}", item.title(), item.meta());
            }
            assert!(!found.items.is_empty());
            assert!(found.items.iter().all(|i| i.matches(kind, "English")));
        }
    }
}
