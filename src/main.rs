mod api;
mod auth;
mod cli;
mod daemon;
mod exec;
mod ipc;
mod model;
mod mpv;
mod paths;
mod resolve;
mod relay;
mod tui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ytmx", version, about = "YouTube Music from the terminal: a fast TUI and a scriptable CLI over YouTube Music's own InnerTube API")]
pub struct Cli {
    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Sign in. By default through the omp browser relay (cookies of your signed-in
    /// music.youtube.com tab); falls back to reading Chrome's cookie files.
    Login {
        /// Force the relay (fail instead of falling back).
        #[arg(long, conflicts_with_all = ["browser", "cookie", "cookie_file"])]
        relay: bool,
        /// Relay CDP URL (default ws://127.0.0.1:9224/cdp, or $YTMX_RELAY).
        #[arg(long)]
        relay_url: Option<String>,
        /// Read cookies from this browser's files instead of the relay:
        /// chrome, chromium, brave, edge, vivaldi, opera, firefox, librewolf.
        #[arg(long)]
        browser: Option<String>,
        /// Browser profile: directory ("Profile 5"), display name or account email.
        /// Default: the profile you used last.
        #[arg(long)]
        profile: Option<String>,
        /// List the browser's profiles and exit.
        #[arg(long)]
        list_profiles: bool,
        /// A raw `Cookie:` header copied from DevTools instead of reading a browser.
        #[arg(long, conflicts_with = "cookie_file")]
        cookie: Option<String>,
        /// File holding a raw Cookie header.
        #[arg(long)]
        cookie_file: Option<std::path::PathBuf>,
        /// Google account index when signed in to several (x-goog-authuser).
        #[arg(long, default_value_t = 0)]
        authuser: u32,
    },
    /// Forget the stored session.
    Logout,
    /// Show the signed-in account.
    Whoami,
    /// Search; prints numbered results usable as refs by other commands.
    Search {
        query: Vec<String>,
        /// songs, videos, albums, artists, playlists (community), featured, episodes, podcasts, profiles
        #[arg(short, long)]
        filter: Option<String>,
        /// Search your library instead of all of YouTube Music.
        #[arg(long)]
        library: bool,
        /// Items per section.
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
    },
    /// Search suggestions as you would see while typing.
    Suggest { query: Vec<String> },
    /// Open a page: home, explore, library, liked, history, charts, moods, new, podcasts,
    /// playlists, songs, albums, artists, subscriptions, profiles, uploads, a ref, an id or a URL.
    #[command(alias = "browse")]
    Open {
        target: Vec<String>,
        #[arg(short = 'n', long, default_value_t = 25)]
        limit: usize,
        /// Follow continuations and load everything.
        #[arg(long)]
        all: bool,
    },
    /// List the actions available on a ref (every menu item the web app offers).
    Actions { target: Option<String> },
    /// Run an action on a ref by number or label, e.g. `ytmx do 3 "save to playlist" --to Mix`.
    Do {
        target: String,
        action: Vec<String>,
        /// Playlist to pick when the action asks for one.
        #[arg(long)]
        to: Option<String>,
        /// Confirm destructive actions.
        #[arg(short, long)]
        yes: bool,
        /// Title when the action creates a playlist.
        #[arg(long)]
        title: Option<String>,
    },
    /// Play a ref, id, URL or search query (no argument: resume).
    Play { target: Vec<String> },
    /// Start a radio from a ref (default: the current track).
    Radio { target: Option<String> },
    /// Add refs to the queue.
    Add {
        targets: Vec<String>,
        /// Play next instead of at the end.
        #[arg(long)]
        next: bool,
    },
    Like { target: Option<String> },
    Dislike { target: Option<String> },
    /// Remove a like or dislike.
    Unlike { target: Option<String> },
    /// Save a ref (default: current track) to one of your playlists.
    Save {
        target: Option<String>,
        #[arg(long)]
        to: String,
    },
    Lyrics { target: Option<String> },
    /// "Related" tab for a track (default: current).
    Related { target: Option<String> },
    Status,
    Queue,
    Pause,
    Resume,
    Toggle,
    Next,
    Prev,
    Stop,
    /// Clear the queue except the current track.
    Clear,
    /// Shuffle the upcoming tracks.
    Shuffle,
    /// Seek: `30`, `-10`, `+15`, or `1:30` / `--to 90` for absolute.
    Seek {
        #[arg(allow_hyphen_values = true)]
        position: String,
        #[arg(long)]
        to: bool,
    },
    /// Volume: `60`, `+5`, `-5`.
    Volume {
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// off, all, one (no argument cycles).
    Repeat { mode: Option<String> },
    /// Keep playing related music when the queue ends: on/off (no argument toggles).
    Autoplay { mode: Option<String> },
    /// Play queue entry N.
    Jump { index: usize },
    /// Remove queue entry N.
    Remove { index: usize },
    /// Move queue entry FROM to TO.
    Move { from: usize, to: usize },
    #[command(subcommand)]
    Playlist(PlaylistCmd),
    Subscribe { target: String },
    Unsubscribe { target: String },
    /// Raw InnerTube call: `ytmx raw browse '{"browseId":"FEmusic_home"}'`.
    Raw { endpoint: String, body: Option<String> },
    /// Stop playback and the background player.
    Quit,
    #[command(hide = true)]
    Daemon,
}

#[derive(Subcommand)]
pub enum PlaylistCmd {
    /// Create a playlist, optionally seeded with refs.
    Create {
        title: String,
        #[arg(short, long, default_value = "")]
        description: String,
        /// public, unlisted, private
        #[arg(short, long, default_value = "private")]
        privacy: String,
        refs: Vec<String>,
    },
    /// Rename, re-describe or change privacy.
    Edit {
        playlist: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        privacy: Option<String>,
    },
    Delete {
        playlist: String,
        #[arg(short, long)]
        yes: bool,
    },
    /// Add refs to a playlist.
    Add { playlist: String, refs: Vec<String> },
    /// Remove refs (numbers from listing that playlist) from it.
    Remove { playlist: String, refs: Vec<String> },
}

fn main() {
    // Behave like a normal Unix tool under `| head`: exit quietly on a closed pipe.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let res = rt.block_on(async move {
        match cli.cmd {
            None => tui::run().await,
            Some(Cmd::Daemon) => daemon::run().await,
            Some(cmd) => cli::run(cmd, cli.json).await,
        }
    });
    if let Err(e) = res {
        eprintln!("ytmx: {e:#}");
        std::process::exit(1);
    }
}
