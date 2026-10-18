use anyhow::{Context, Result};
use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub abb: AbbConfig,
    pub qbittorrent: QbitConfig,
    pub libgen: LibgenConfig,
    pub search: SearchConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AbbConfig {
    /// AudiobookBay changes domains often, so keep it configurable.
    pub base_url: String,
    /// Optional: the site currently shows info hashes without logging in.
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QbitConfig {
    /// "auto" tries the Web UI first and falls back to the qbittorrent binary,
    /// "webui" uses only the Web UI API, "cli" only the binary.
    pub mode: String,
    pub webui_url: String,
    pub username: String,
    pub password: String,
    pub category: String,
    pub save_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LibgenConfig {
    /// Library Genesis mirror used for ebooks (libgen.li, libgen.gl, libgen.la, libgen.bz…).
    pub base_url: String,
    pub save_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// Language filter at startup; "All" shows every language.
    pub language: String,
    /// Languages cycled through with L / Ctrl+L, after "All".
    pub languages: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            abb: AbbConfig::default(),
            qbittorrent: QbitConfig::default(),
            libgen: LibgenConfig::default(),
            search: SearchConfig::default(),
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        let languages = ["English", "Serbian", "Croatian", "Bosnian", "German", "French", "Spanish", "Italian", "Russian"];
        Self { language: "All".into(), languages: languages.map(String::from).to_vec() }
    }
}

impl Default for LibgenConfig {
    fn default() -> Self {
        Self { base_url: "https://libgen.li".into(), save_path: documents_subdir("Ebooks") }
    }
}

impl Default for AbbConfig {
    fn default() -> Self {
        Self {
            base_url: "https://audiobookbay.lu".into(),
            username: String::new(),
            password: String::new(),
        }
    }
}

impl Default for QbitConfig {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            webui_url: "http://localhost:8080".into(),
            username: String::new(),
            password: String::new(),
            category: "audiobooks".into(),
            save_path: documents_subdir("Audiobooks"),
        }
    }
}

fn documents_subdir(name: &str) -> String {
    UserDirs::new()
        .and_then(|d| d.document_dir().map(|p| p.join(name).to_string_lossy().into_owned()))
        .unwrap_or_else(|| format!("~/Documents/{name}"))
}

pub fn config_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "bookgrab").context("cannot determine config directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// Loads the config, writing a default one on first run so users can find and edit it.
pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        let config = Config::default();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, toml::to_string_pretty(&config)?)?;
        return Ok(config);
    }
    let text = std::fs::read_to_string(&path)?;
    let config: Config = toml::from_str(&text).with_context(|| format!("invalid config at {}", path.display()))?;
    append_missing_sections(&path, &text, &config)?;
    Ok(config)
}

/// Sections added in newer versions get appended so users can see and edit them,
/// without rewriting (and losing comments in) the rest of the file.
fn append_missing_sections(path: &std::path::Path, text: &str, config: &Config) -> Result<()> {
    let existing: toml::Table = toml::from_str(text)?;
    let full = toml::Table::try_from(config)?;
    let mut missing = toml::Table::new();
    for (key, value) in full {
        if !existing.contains_key(&key) {
            missing.insert(key, value);
        }
    }
    if !missing.is_empty() {
        let mut text = text.trim_end().to_string();
        text.push_str("\n\n");
        text.push_str(&toml::to_string_pretty(&missing)?);
        std::fs::write(path, text)?;
    }
    Ok(())
}

/// Expands a leading `~/` so paths in the config can be written either way.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), UserDirs::new()) {
        (Some(rest), Some(dirs)) => dirs.home_dir().join(rest),
        _ => PathBuf::from(path),
    }
}
