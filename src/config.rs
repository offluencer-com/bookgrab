use anyhow::{Context, Result};
use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub abb: AbbConfig,
    pub qbittorrent: QbitConfig,
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

impl Default for Config {
    fn default() -> Self {
        Self { abb: AbbConfig::default(), qbittorrent: QbitConfig::default() }
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
        let save_path = UserDirs::new()
            .and_then(|d| d.document_dir().map(|p| p.join("Audiobooks")))
            .unwrap_or_else(|| PathBuf::from("~/Documents/Audiobooks"));
        Self {
            mode: "auto".into(),
            webui_url: "http://localhost:8080".into(),
            username: String::new(),
            password: String::new(),
            category: "audiobooks".into(),
            save_path: save_path.to_string_lossy().into_owned(),
        }
    }
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
    toml::from_str(&text).with_context(|| format!("invalid config at {}", path.display()))
}

/// Expands a leading `~/` so paths in the config can be written either way.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), UserDirs::new()) {
        (Some(rest), Some(dirs)) => dirs.home_dir().join(rest),
        _ => PathBuf::from(path),
    }
}
