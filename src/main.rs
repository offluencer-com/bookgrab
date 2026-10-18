mod abb;
mod config;
mod torrent;
mod tui;

use anyhow::Result;
use ratatui_image::picker::Picker;

const HELP: &str = "\
bookgrab - search AudiobookBay and send audiobooks to qBittorrent

USAGE:
    bookgrab [QUERY...]     open the TUI, optionally searching for QUERY
    bookgrab --config       print the config file path
    bookgrab --help         show this help
    bookgrab --version      show the version
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            print!("{HELP}");
            return Ok(());
        }
        Some("-V" | "--version") => {
            println!("bookgrab {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--config") => {
            config::load()?;
            println!("{}", config::config_path()?.display());
            return Ok(());
        }
        _ => {}
    }

    let config = config::load()?;
    let query = (!args.is_empty()).then(|| args.join(" "));

    // Detect kitty / sixel / iTerm2 support before the TUI takes over the terminal;
    // terminals without any graphics protocol get unicode half-blocks.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());

    tui::run(config, picker, query)
}
