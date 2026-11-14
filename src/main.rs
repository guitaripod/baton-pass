mod config;
mod crypto;
mod proto;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ferry", version, about = "Carry this desktop's mouse and keyboard over to a Mac")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: the capturing side on Linux, the injecting side on macOS
    Run,
    /// Create the shared key if it does not exist yet and print its path
    Keygen,
    /// Switch the mouse and keyboard to the Mac, or back
    #[cfg(target_os = "linux")]
    Toggle,
    /// Show the link, capture and Mac state
    #[cfg(target_os = "linux")]
    Status,
    /// Install the user service, the KWin hotkey script and the default config
    #[cfg(target_os = "linux")]
    Install {
        /// The Mac's address, repeatable, tried in order (only used when no config exists yet)
        #[arg(long = "peer")]
        peers: Vec<String>,
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
    ClipSet {
        #[arg(long)]
        mime: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
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
        Command::Keygen => {
            let path = config::Config::key_path();
            crypto::ensure_key(&path)?;
            println!("{}", path.display());
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Command::Toggle => local.block_on(&runtime, linux::control::toggle()),
        #[cfg(target_os = "linux")]
        Command::Status => local.block_on(&runtime, linux::control::status()),
        #[cfg(target_os = "linux")]
        Command::Install { peers } => local.block_on(&runtime, linux::install::install(peers)),
        #[cfg(target_os = "linux")]
        Command::Probe(args) => local.block_on(&runtime, linux::probe::probe(args)),
        #[cfg(target_os = "macos")]
        Command::ClipGet => macos::pasteboard::helper_get(),
        #[cfg(target_os = "macos")]
        Command::ClipSet { mime } => macos::pasteboard::helper_set(&mime),
    }
}
