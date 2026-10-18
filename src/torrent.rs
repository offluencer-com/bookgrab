//! Hands magnet links to qBittorrent, via the Web UI API or the desktop binary.

use crate::config::{QbitConfig, expand_home};
use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Adds the magnet to qBittorrent and returns a short description of how it was sent.
pub fn add_magnet(cfg: &QbitConfig, magnet: &str) -> Result<String> {
    let save_path = expand_home(&cfg.save_path);
    std::fs::create_dir_all(&save_path)
        .with_context(|| format!("cannot create {}", save_path.display()))?;
    let save_path = save_path.to_string_lossy();

    match cfg.mode.as_str() {
        "webui" => add_via_webui(cfg, magnet, &save_path).map(|_| "qBittorrent Web UI".into()),
        "cli" => add_via_binary(cfg, magnet, &save_path).map(|_| "qBittorrent app".into()),
        _ => match add_via_webui(cfg, magnet, &save_path) {
            Ok(()) => Ok("qBittorrent Web UI".into()),
            Err(webui_err) => add_via_binary(cfg, magnet, &save_path)
                .map(|_| "qBittorrent app".into())
                .map_err(|e| e.context(format!("Web UI also failed: {webui_err:#}"))),
        },
    }
}

fn add_via_webui(cfg: &QbitConfig, magnet: &str, save_path: &str) -> Result<()> {
    let base = cfg.webui_url.trim_end_matches('/');
    let http = Client::builder().cookie_store(true).timeout(Duration::from_secs(5)).build()?;

    // Not needed when "bypass authentication for clients on localhost" is enabled.
    if !cfg.username.is_empty() {
        let resp = http
            .post(format!("{base}/api/v2/auth/login"))
            .header("Referer", base)
            .form(&[("username", cfg.username.as_str()), ("password", cfg.password.as_str())])
            .send()?
            .text()?;
        if resp.trim() != "Ok." {
            bail!("qBittorrent Web UI login rejected");
        }
    }

    let resp = http
        .post(format!("{base}/api/v2/torrents/add"))
        .header("Referer", base)
        .form(&[("urls", magnet), ("savepath", save_path), ("category", cfg.category.as_str())])
        .send()?;
    let status = resp.status();
    let body = resp.text()?;
    if !status.is_success() || body.trim() == "Fails." {
        bail!("qBittorrent Web UI refused the torrent ({status}: {})", body.trim());
    }
    Ok(())
}

/// The qbittorrent binary forwards arguments to an already running instance, or starts one.
fn add_via_binary(cfg: &QbitConfig, magnet: &str, save_path: &str) -> Result<()> {
    let mut cmd = Command::new("qbittorrent");
    cmd.arg(format!("--save-path={save_path}")).arg("--skip-dialog=true");
    if !cfg.category.is_empty() {
        cmd.arg(format!("--category={}", cfg.category));
    }
    let mut child = cmd
        .arg(magnet)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("could not run `qbittorrent` (is it installed?)")?;
    // Reap the process in the background so it doesn't linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}
