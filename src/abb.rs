//! AudiobookBay client: search, book details and magnet links.

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use scraper::{ElementRef, Html, Selector};
use std::time::Duration;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";

#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub cover_url: Option<String>,
    pub category: String,
    pub language: String,
    pub posted: String,
    pub format: String,
    pub bitrate: String,
    pub size: String,
}

#[derive(Debug, Clone, Default)]
pub struct SearchPage {
    pub results: Vec<SearchResult>,
    pub has_next: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BookDetails {
    pub title: String,
    pub authors: Vec<String>,
    pub narrators: Vec<String>,
    pub format: String,
    pub size: String,
    pub description: String,
    pub info_hash: String,
    pub trackers: Vec<String>,
}

impl BookDetails {
    pub fn magnet(&self) -> String {
        let mut magnet = format!(
            "magnet:?xt=urn:btih:{}&dn={}",
            self.info_hash,
            urlencoding::encode(&self.title)
        );
        for tracker in &self.trackers {
            magnet.push_str("&tr=");
            magnet.push_str(&urlencoding::encode(tracker));
        }
        magnet
    }
}

pub struct AbbClient {
    http: Client,
    base_url: String,
}

impl AbbClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .cookie_store(true)
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, base_url: base_url.trim_end_matches('/').to_string() })
    }

    pub fn login(&self, username: &str, password: &str) -> Result<()> {
        let url = format!("{}/member/login.php", self.base_url);
        let body = self
            .http
            .post(&url)
            .form(&[("username", username), ("password", password)])
            .send()?
            .error_for_status()?
            .text()?;
        if body.contains("member/login.php") && body.contains("name=\"password\"") {
            bail!("AudiobookBay login failed, check username/password in the config");
        }
        Ok(())
    }

    pub fn search(&self, query: &str, page: u32) -> Result<SearchPage> {
        let query = urlencoding::encode(&query.trim().to_lowercase()).into_owned();
        let url = if page <= 1 {
            format!("{}/?s={query}&tt=1", self.base_url)
        } else {
            format!("{}/page/{page}/?s={query}&tt=1", self.base_url)
        };
        let html = self.get_text(&url)?;
        Ok(parse_search(&html, &self.base_url))
    }

    pub fn details(&self, url: &str) -> Result<BookDetails> {
        let html = self.get_text(url)?;
        let details = parse_details(&html);
        if details.info_hash.is_empty() {
            bail!("no info hash found on the page (the site may require login)");
        }
        Ok(details)
    }

    pub fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self.http.get(url).send()?.error_for_status()?;
        Ok(resp.bytes()?.to_vec())
    }

    fn get_text(&self, url: &str) -> Result<String> {
        let resp = self.http.get(url).send().with_context(|| format!("request to {url} failed"))?;
        Ok(resp.error_for_status()?.text()?)
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
    if url.starts_with("http") { url.to_string() } else { format!("{base}{url}") }
}

/// Returns the text after `label` up to the next line break, from a flattened block of text.
fn field_after(text: &str, label: &str) -> String {
    text.split_once(label)
        .map(|(_, rest)| rest.lines().next().unwrap_or("").split('/').next().unwrap_or(""))
        .unwrap_or("")
        .replace('\u{a0}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_search(html: &str, base: &str) -> SearchPage {
    let doc = Html::parse_document(html);
    let (post, title, info, img, content) =
        (sel("div.post"), sel(".postTitle a"), sel(".postInfo"), sel(".postContent img"), sel(".postContent > p"));

    let mut results = Vec::new();
    for el in doc.select(&post) {
        let Some(link) = el.select(&title).next() else { continue };
        let Some(href) = link.value().attr("href") else { continue };

        // postInfo and the stats paragraph use <br> separators; keep them as line breaks.
        let info_text = block_text(el.select(&info).next());
        let stats_text = el.select(&content).map(|p| block_text(Some(p))).collect::<Vec<_>>().join("\n");

        results.push(SearchResult {
            title: text_of(link),
            url: absolute(href, base),
            cover_url: el.select(&img).next().and_then(|i| i.value().attr("src")).map(|s| absolute(s, base)),
            category: field_after(&info_text, "Category:"),
            language: field_after(&info_text, "Language:").split("Keywords").next().unwrap_or("").trim().to_string(),
            posted: field_after(&stats_text, "Posted:"),
            format: field_after(&stats_text, "Format:"),
            bitrate: field_after(&stats_text, "Bitrate:"),
            size: field_after(&stats_text, "File Size:"),
        });
    }

    let has_next = doc.select(&sel(".wp-pagenavi a")).any(|a| text_of(a) == "»");
    SearchPage { results, has_next }
}

/// Collects text of an element, turning <br> into newlines.
fn block_text(el: Option<ElementRef>) -> String {
    let Some(el) = el else { return String::new() };
    let mut out = String::new();
    for node in el.descendants() {
        if let Some(t) = node.value().as_text() {
            out.push_str(t);
        } else if node.value().as_element().is_some_and(|e| e.name() == "br") {
            out.push('\n');
        }
    }
    out
}

pub fn parse_details(html: &str) -> BookDetails {
    let doc = Html::parse_document(html);
    let mut d = BookDetails {
        title: doc.select(&sel(".postTitle h1")).next().map(text_of).unwrap_or_default(),
        authors: doc.select(&sel(".desc .author")).map(text_of).collect(),
        narrators: doc.select(&sel(".desc .narrator")).map(text_of).collect(),
        format: doc.select(&sel(".desc .format")).next().map(text_of).unwrap_or_default(),
        ..Default::default()
    };

    // Skip the first paragraph: it repeats author/narrator/format.
    let paragraphs: Vec<String> =
        doc.select(&sel(".desc > p")).skip(1).map(text_of).filter(|p| !p.is_empty()).collect();
    d.description = paragraphs.join("\n\n");

    let td = sel("td");
    for row in doc.select(&sel("tr")) {
        let cells: Vec<String> = row.select(&td).map(text_of).collect();
        let [label, value, ..] = cells.as_slice() else { continue };
        match label.as_str() {
            "Info Hash:" => d.info_hash = value.to_lowercase(),
            "Tracker:" | "Announce URL:" if !d.trackers.contains(value) => d.trackers.push(value.clone()),
            "File Size:" => d.size = value.clone(),
            _ => {}
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://audiobookbay.lu";

    #[test]
    fn parses_search_results() {
        let html = r#"<div class="post"><div class="postTitle"><h2><a href="/abss/dune/">Dune - Frank Herbert</a></h2></div>
            <div class="postInfo">Category: Classic&nbsp; Sci-Fi&nbsp; <br />Language: English<span>Keywords: Space&nbsp </span><br /></div>
            <div class="postContent"><div class="center"><p><a href="/abss/dune/"><img src="https://img.example/dune.jpg" /></a></p></div>
            <p style='text-align:center;'>Posted: 1 May 2026<br />Format: <span>M4B</span> / Bitrate: <span>64 Kbps</span><br />File Size: <span>1.12</span> GBs</p></div></div>
            <div class="wp-pagenavi"><span class="current">1</span><a href="/page/2/?s=dune">2</a><a href="/page/2/?s=dune">&raquo;</a></div>"#;
        let page = parse_search(html, BASE);
        assert!(page.has_next);
        let r = &page.results[0];
        assert_eq!(r.title, "Dune - Frank Herbert");
        assert_eq!(r.url, "https://audiobookbay.lu/abss/dune/");
        assert_eq!(r.cover_url.as_deref(), Some("https://img.example/dune.jpg"));
        assert_eq!(r.category, "Classic Sci-Fi");
        assert_eq!(r.language, "English");
        assert_eq!(r.posted, "1 May 2026");
        assert_eq!(r.format, "M4B");
        assert_eq!(r.bitrate, "64 Kbps");
        assert_eq!(r.size, "1.12 GBs");
    }

    #[test]
    fn parses_details_and_builds_magnet() {
        let html = r#"<div class="postTitle"><h1>Dune - Frank Herbert</h1></div>
            <div class="postContent"><img src="https://img.example/dune.jpg" itemprop="image" />
            <div class="desc"><p>Written by <span class="author">Frank Herbert</span> Read by <span class="narrator">Scott Brick</span> Format: <span class="format">M4B</span></p>
            <p>Set on the desert planet Arrakis.</p></div>
            <table><tr><td>Announce URL:</td><td>http://t1/announce</td></tr>
            <tr><td>Tracker:</td><td>http://t1/announce</td></tr>
            <tr><td>Tracker:</td><td>udp://t2:6969/announce</td></tr>
            <tr><td>File Size:</td><td><span>1.12</span> GBs</td></tr>
            <tr><td>Info Hash:</td><td>061850EAD3EB6F1C5C6D8420211B4BBF2D4EE3E2</td></tr></table></div>"#;
        let d = parse_details(html);
        assert_eq!(d.authors, vec!["Frank Herbert"]);
        assert_eq!(d.narrators, vec!["Scott Brick"]);
        assert_eq!(d.description, "Set on the desert planet Arrakis.");
        assert_eq!(d.trackers.len(), 2);
        assert_eq!(d.size, "1.12 GBs");
        assert_eq!(
            d.magnet(),
            "magnet:?xt=urn:btih:061850ead3eb6f1c5c6d8420211b4bbf2d4ee3e2&dn=Dune%20-%20Frank%20Herbert\
             &tr=http%3A%2F%2Ft1%2Fannounce&tr=udp%3A%2F%2Ft2%3A6969%2Fannounce"
        );
    }

    /// Hits the real site; run with `cargo test -- --ignored --nocapture` to check
    /// that the HTML layout hasn't changed.
    #[test]
    #[ignore]
    fn live_search_and_details() {
        let client = AbbClient::new(BASE).unwrap();
        let page = client.search("dune", 1).unwrap();
        assert!(!page.results.is_empty());
        for r in &page.results {
            println!("{r:?}");
        }
        let details = client.details(&page.results[0].url).unwrap();
        println!("{details:#?}\n{}", details.magnet());
        assert_eq!(details.info_hash.len(), 40);
    }
}
