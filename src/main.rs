mod abb;
mod config;
mod libgen;
mod torrent;
mod tui;

use anyhow::Result;
use ratatui_image::picker::Picker;
use tui::Kind;

const HELP: &str = "\
bookgrab - find audiobooks (AudiobookBay → qBittorrent) and ebooks (LibGen)

USAGE:
    bookgrab [TYPE] [QUERY...]   open the TUI, optionally searching for QUERY right away
    bookgrab --config            print the config file path
    bookgrab --help              show this help
    bookgrab --version           show the version

TYPE (default: audiobook; switch any time with Tab in the app):
    -a, --audio     audiobooks from AudiobookBay
    -e, --epub      EPUB ebooks from LibGen
    -p, --pdf       PDF ebooks from LibGen
";

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
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

    let mut kind = Kind::Audiobook;
    args.retain(|arg| {
        let flag = match arg.as_str() {
            "-a" | "--audio" | "--audiobook" => Some(Kind::Audiobook),
            "-e" | "--epub" => Some(Kind::Epub),
            "-p" | "--pdf" => Some(Kind::Pdf),
            _ => None,
        };
        if let Some(k) = flag {
            kind = k;
        }
        flag.is_none()
    });

    let config = config::load()?;
    let query = (!args.is_empty()).then(|| args.join(" "));

    // Detect kitty / sixel / iTerm2 support before the TUI takes over the terminal;
    // terminals without any graphics protocol get unicode half-blocks.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());

    tui::run(config, picker, kind, query)
}
