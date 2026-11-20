mod clip;
mod config;
mod crypto;
#[cfg(any(target_os = "macos", test))]
mod jitter;
mod pair;
mod proto;
mod session;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "baton-pass", version, about = "Carry this desktop's mouse and keyboard over to a Mac")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: the capturing side on Linux, the injecting side on macOS
    Run,
    /// Put a fresh shared key on both machines: run `sudo baton-pass pair` on the Mac first, then
    /// `baton-pass pair <code>` here with the code it shows
    #[cfg(target_os = "linux")]
    Pair {
        /// The eight-digit code `sudo baton-pass pair` shows on the Mac
        code: String,
        /// The Mac's address, if not the first configured peer
        #[arg(long)]
        peer: Option<String>,
    },
    /// Show a pairing code and wait for the desktop to run `baton-pass pair <code>`
    #[cfg(target_os = "macos")]
    Pair,
    /// Switch the mouse and keyboard to the Mac, or back
    #[cfg(target_os = "linux")]
    Toggle,
    /// Show the link, capture and Mac state
    #[cfg(target_os = "linux")]
    Status,
    /// Install the user service, the KWin hotkey script and the default config
    #[cfg(target_os = "linux")]
    Install {
        /// The Mac's address, repeatable, tried in order; replaces the peers of an existing config
        #[arg(long = "peer")]
        peers: Vec<String>,
    },
    /// Copy this binary to /usr/local/bin and run it as a LaunchDaemon (run with sudo)
    #[cfg(target_os = "macos")]
    Install,
    /// Stop the daemon and remove what install put in place (sudo on the Mac)
    Uninstall {
        /// Also remove the config and the shared key
        #[arg(long)]
        purge: bool,
    },
    /// Check the link and the Mac side without touching local input
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    Probe(linux::probe::ProbeArgs),
    #[cfg(target_os = "macos")]
    #[command(hide = true)]
    ClipGet,
    #[cfg(target_os = "macos")]
    #[command(hide = true)]
    ClipSet,
}

fn main() -> Result<()> {
    let (log_writer, _log_flush) = tracing_appender::non_blocking(std::io::stderr());
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(log_writer)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();

    match cli.command {
        Command::Run => {
            let config = config::Config::load()?;
            #[cfg(target_os = "linux")]
            return local.block_on(&runtime, linux::run(config));
            #[cfg(target_os = "macos")]
            return local.block_on(&runtime, macos::run(config));
        }
        #[cfg(target_os = "linux")]
        Command::Pair { code, peer } => local.block_on(&runtime, pair::on_desktop(&code, peer)),
        #[cfg(target_os = "macos")]
        Command::Pair => local.block_on(&runtime, pair::on_mac()),
        #[cfg(target_os = "linux")]
        Command::Toggle => local.block_on(&runtime, linux::control::toggle()),
        #[cfg(target_os = "linux")]
        Command::Status => local.block_on(&runtime, linux::control::status()),
        #[cfg(target_os = "linux")]
        Command::Install { peers } => local.block_on(&runtime, linux::install::install(peers)),
        #[cfg(target_os = "linux")]
        Command::Uninstall { purge } => local.block_on(&runtime, linux::install::uninstall(purge)),
        #[cfg(target_os = "macos")]
        Command::Install => macos::install::install(),
        #[cfg(target_os = "macos")]
        Command::Uninstall { purge } => macos::install::uninstall(purge),
        #[cfg(target_os = "linux")]
        Command::Probe(args) => local.block_on(&runtime, linux::probe::probe(args)),
        #[cfg(target_os = "macos")]
        Command::ClipGet => macos::pasteboard::helper_get(),
        #[cfg(target_os = "macos")]
        Command::ClipSet => macos::pasteboard::helper_set(),
    }
}
