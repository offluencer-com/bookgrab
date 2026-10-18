//! Library Genesis client: ebook search filtered by file type, and direct downloads.

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::header::REFERER;
use scraper::{ElementRef, Html, Selector};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";
/// LibGen can't filter by extension, so pages (25 results each, several seconds per
/// request) are filtered locally; this many are tried before giving up on finding a match.
const MAX_PAGES_PER_SEARCH: u32 = 3;

#[derive(Debug, Clone, Default)]
pub struct Ebook {
    pub title: String,
    pub series: String,
    pub authors: String,
    pub publisher: String,
    pub year: String,
    pub language: String,
    pub pages: String,
    pub size: String,
    pub extension: String,
    pub md5: String,
    pub cover_url: Option<String>,
}

impl Ebook {
    /// "Author - Title.ext", safe to use as a file name.
    pub fn file_name(&self) -> String {
        let author = main_author(&self.authors);
        let stem = if author.is_empty() { self.title.clone() } else { format!("{author} - {}", self.title) };
        let clean: String = stem
            .chars()
            .map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '_' } else { c })
            .take(150)
            .collect();
        format!("{}.{}", clean.trim(), self.extension)
    }
}

/// First author in readable order: "Herbert, Brian(aut) [1962-]; Anderson…" → "Brian Herbert".
fn main_author(authors: &str) -> String {
    let first = authors.split(';').next().unwrap_or("");
    // Drop annotations like "(aut)" and "[1962-]".
    let mut clean = String::new();
    let mut depth = 0;
    for c in first.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            _ if depth == 0 => clean.push(c),
            _ => {}
        }
    }
    let clean = clean.trim().trim_end_matches(',').trim();
    match clean.split_once(',') {
        // "Last, First" (a single-word surname) becomes "First Last".
        Some((last, given)) if !last.trim().contains(' ') && !given.contains(',') => {
            format!("{} {}", given.trim(), last.trim())
        }
        // Otherwise a comma separates several authors: keep the first.
        Some((first, _)) => first.trim().to_string(),
        None => clean.to_string(),
    }
}

#[derive(Debug, Clone, Default)]
pub struct EbookPage {
    pub results: Vec<Ebook>,
    /// The server page these results came from (may be past the requested one).
    pub page: u32,
    pub total_pages: u32,
}

pub struct LibgenClient {
    http: Client,
    base_url: String,
}

impl LibgenClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let http = Client::builder().user_agent(USER_AGENT).timeout(Duration::from_secs(60)).build()?;
        Ok(Self { http, base_url: base_url.trim_end_matches('/').to_string() })
    }

    /// Searches for `query`, keeping only files with `extension`. Skips ahead over
    /// server pages that have no matching files.
    pub fn search(&self, query: &str, extension: &str, page: u32) -> Result<EbookPage> {
        let mut page = page.max(1);
        let last_try = page + MAX_PAGES_PER_SEARCH - 1;
        loop {
            let url = format!(
                "{}/index.php?req={}&columns%5B%5D=t&columns%5B%5D=a&columns%5B%5D=s\
                 &objects%5B%5D=f&topics%5B%5D=l&topics%5B%5D=f&covers=on&filesuns=all&page={page}",
                self.base_url,
                urlencoding::encode(query.trim())
            );
            let html = self.http.get(&url).send()?.error_for_status()?.text()?;
            let mut found = parse_search(&html, &self.base_url);
            found.results.retain(|b| b.extension.eq_ignore_ascii_case(extension));
            found.page = page;
            if !found.results.is_empty() || page >= found.total_pages || page >= last_try {
                return Ok(found);
            }
            page += 1;
        }
    }

    /// LibGen serves covers only with a Referer from its own site.
    pub fn fetch_cover(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self.http.get(url).header(REFERER, format!("{}/", self.base_url)).send()?;
        let bytes = resp.error_for_status()?.bytes()?;
        if bytes.is_empty() {
            bail!("empty cover");
        }
        Ok(bytes.to_vec())
    }

    /// Downloads the book into `dir`, calling `progress(downloaded, total)` as it goes.
    pub fn download(
        &self,
        book: &Ebook,
        dir: &Path,
        mut progress: impl FnMut(u64, Option<u64>),
    ) -> Result<PathBuf> {
        let link = self.download_link(&book.md5)?;
        let mut resp = self.http.get(&link).timeout(Duration::from_secs(60 * 30)).send()?.error_for_status()?;
        let total = resp.content_length();

        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(book.file_name());
        let part = path.with_extension(format!("{}.part", book.extension));
        let mut file = std::fs::File::create(&part)?;

        let mut buf = vec![0u8; 64 * 1024];
        let mut downloaded = 0u64;
        loop {
            let n = resp.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            downloaded += n as u64;
            progress(downloaded, total);
        }
        file.flush()?;
        std::fs::rename(&part, &path)?;
        Ok(path)
    }

    /// The download page (ads.php) holds a get.php link with a short-lived key.
    fn download_link(&self, md5: &str) -> Result<String> {
        let html = self
            .http
            .get(format!("{}/ads.php?md5={md5}", self.base_url))
            .send()?
            .error_for_status()?
            .text()?;
        let doc = Html::parse_document(&html);
        let href = doc
            .select(&sel("a[href*='get.php?md5=']"))
            .find_map(|a| a.value().attr("href"))
            .context("no download link on the LibGen page")?;
        Ok(absolute(href, &self.base_url))
    }
}

fn sel(css: &str) -> Selector {
    Selector::parse(css).expect("valid selector")
}

fn text_of(el: ElementRef) -> String {
    let joined: String = el.text().collect();
    joined.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn absolute(url: &str, base: &str) -> String {
    if url.starts_with("http") {
        url.to_string()
    } else {
        format!("{base}/{}", url.trim_start_matches('/'))
    }
}

/// Page counts come as "0", "320" or "0 / 524 pages; 25 cm."; keep just the number.
fn page_count(raw: &str) -> String {
    raw.split('/')
        .map(|part| part.trim().chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .find(|n| !n.is_empty() && n != "0")
        .unwrap_or_default()
}

pub fn parse_search(html: &str, base: &str) -> EbookPage {
    let doc = Html::parse_document(html);
    let (row_sel, td_sel) = (sel("#tablelibgen > tbody > tr"), sel(":scope > td"));
    let (img_sel, series_sel) = (sel("img"), sel("b"));
    let (title_sel, mirror_sel) = (sel("a[href*='edition.php']"), sel("a[href*='md5=']"));

    let mut results = Vec::new();
    for row in doc.select(&row_sel) {
        let cells: Vec<ElementRef> = row.select(&td_sel).collect();
        if cells.len() < 10 {
            continue;
        }
        let md5 = row
            .select(&mirror_sel)
            .filter_map(|a| a.value().attr("href"))
            .find_map(|href| href.split("md5=").nth(1))
            .map(|m| m.chars().take(32).collect::<String>().to_lowercase());
        let Some(md5) = md5 else { continue };

        let title = cells[1].select(&title_sel).map(text_of).find(|t| !t.is_empty()).unwrap_or_default();
        let pages = page_count(&text_of(cells[6]));
        results.push(Ebook {
            title,
            series: cells[1].select(&series_sel).next().map(text_of).unwrap_or_default(),
            authors: text_of(cells[2]).trim_end_matches([',', ' ']).to_string(),
            publisher: text_of(cells[3]),
            year: text_of(cells[4]),
            language: text_of(cells[5]),
            pages,
            size: text_of(cells[7]),
            extension: text_of(cells[8]).to_lowercase(),
            md5,
            // The list shows thumbnails; the full-size cover sits next to them.
            cover_url: cells[0]
                .select(&img_sel)
                .next()
                .and_then(|i| i.value().attr("src"))
                .map(|s| absolute(&s.replace("_small", ""), base)),
        });
    }

    // Pagination is rendered by JS: new Paginator("paginator_example_top", <pages>, ...)
    let total_pages = html
        .split("Paginator(\"paginator_example_top\",")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(1);
    EbookPage { results, page: 1, total_pages }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://libgen.li";

    #[test]
    fn parses_results() {
        let html = r#"<script>p = new Paginator("paginator_example_top", 20, 25, 1, "/index.php?page=");</script>
            <table id="tablelibgen"><thead><tr><th></th></tr></thead><tbody><tr>
            <td><a href="edition.php?id=1"><img src="/fictioncovers/2509000/24778aacb1d0844950bf463c145b3d21_small.jpg"></a></td>
            <td><b>Dune 1-6</b><br><a title="tooltip" href="edition.php?id=1">Frank Herbert's Dune Saga Collection <i></i></a>
                <nobr><span class="badge">b</span></nobr></td>
            <td>Frank Herbert,  </td><td>Penguin</td><td><nobr>2020</nobr></td><td>English</td><td>0</td>
            <td><nobr><a href="/file.php?id=1">3 MB</a></nobr></td><td>epub</td>
            <td><a title="libgen" href="/ads.php?md5=24778AACB1D0844950BF463C145B3D21"><span>1</span></a></td>
            </tr></tbody></table>"#;
        let page = parse_search(html, BASE);
        assert_eq!(page.total_pages, 20);
        let b = &page.results[0];
        assert_eq!(b.title, "Frank Herbert's Dune Saga Collection");
        assert_eq!(b.series, "Dune 1-6");
        assert_eq!(b.authors, "Frank Herbert");
        assert_eq!(b.year, "2020");
        assert_eq!(b.pages, "");
        assert_eq!(b.size, "3 MB");
        assert_eq!(b.extension, "epub");
        assert_eq!(b.md5, "24778aacb1d0844950bf463c145b3d21");
        assert_eq!(
            b.cover_url.as_deref(),
            Some("https://libgen.li/fictioncovers/2509000/24778aacb1d0844950bf463c145b3d21.jpg")
        );
        assert_eq!(b.file_name(), "Frank Herbert - Frank Herbert's Dune Saga Collection.epub");
    }

    #[test]
    fn picks_readable_main_author() {
        assert_eq!(main_author("Austen, Jane"), "Jane Austen");
        assert_eq!(main_author("Herbert, Brian(aut); Anderson, Kevin J.(aut) [1962-]"), "Brian Herbert");
        assert_eq!(main_author("Frank Herbert, Brian Herbert"), "Frank Herbert");
        assert_eq!(main_author("Frank Herbert"), "Frank Herbert");
        assert_eq!(main_author(""), "");
    }

    #[test]
    fn cleans_page_counts() {
        assert_eq!(page_count("0"), "");
        assert_eq!(page_count("320"), "320");
        assert_eq!(page_count("0 / 524 pages; 25 cm."), "524");
    }

    #[test]
    fn file_name_strips_unsafe_characters() {
        let book = Ebook { title: "What/Why: A <Guide>?".into(), extension: "pdf".into(), ..Default::default() };
        assert_eq!(book.file_name(), "What_Why_ A _Guide__.pdf");
    }

    /// Hits the real site; run with `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_search() {
        let client = LibgenClient::new(BASE).unwrap();
        for ext in ["epub", "pdf"] {
            let page = client.search("dune herbert", ext, 1).unwrap();
            println!("{ext}: page {} of {}, {} results", page.page, page.total_pages, page.results.len());
            for b in page.results.iter().take(3) {
                println!("  {b:?}");
            }
            assert!(page.results.iter().all(|b| b.extension == ext));
            assert!(!page.results.is_empty());
        }
    }

    /// Downloads a small public-domain book to a temp dir and checks it's a real EPUB.
    #[test]
    #[ignore]
    fn live_download() {
        let client = LibgenClient::new(BASE).unwrap();
        let page = client.search("pride and prejudice austen", "epub", 1).unwrap();
        let book = page.results.iter().min_by_key(|b| b.size.contains("MB")).unwrap();
        let dir = std::env::temp_dir().join("bookgrab-test");
        let path = client.download(book, &dir, |_, _| {}).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        println!("{} ({} bytes)", path.display(), bytes.len());
        assert_eq!(&bytes[..2], b"PK", "EPUB files are zip archives");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
