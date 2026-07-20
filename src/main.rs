mod client;
mod protocol;
mod server;
mod session;
mod termstate;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use session::{generate_session_name, session_base_dir};

#[derive(Debug, Parser)]
#[command(
    name = "reshell",
    version,
    about = "Keep shells alive across SSH disconnects with explicit attach/detach sessions"
)]
struct Cli {
    /// Override session storage directory (default: $XDG_RUNTIME_DIR/reshell)
    #[arg(long, global = true, env = "RESHELL_DIR")]
    dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create a new session and attach to it
    New {
        /// Session name (generated if omitted)
        name: Option<String>,
        /// Shell to run (default: /bin/zsh)
        #[arg(long)]
        shell: Option<String>,
        /// Create the session without attaching
        #[arg(long, short = 'd')]
        detach: bool,
    },
    /// Attach to an existing session (most recently active if name omitted)
    Attach {
        /// Session name (defaults to the most recently active session)
        name: Option<String>,
    },
    /// List running sessions
    List,
    /// Terminate a session and its shell
    Kill {
        /// Session name
        name: String,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("reshell: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let base = match cli.dir {
        Some(d) => d,
        None => session_base_dir()?,
    };

    match cli.command {
        Commands::New {
            name,
            shell,
            detach,
        } => {
            let name = name.unwrap_or_else(generate_session_name);
            let shell = shell.unwrap_or_else(default_shell);
            server::create_session(server::NewSessionOpts {
                name: name.clone(),
                shell,
                base: base.clone(),
            })?;
            if detach {
                println!("{name}");
                Ok(())
            } else {
                // Name goes to stderr so it does not collide with the TTY session.
                eprintln!("{name}");
                client::attach(&base, &name)
            }
        }
        Commands::Attach { name } => {
            let name = match name {
                Some(n) => n,
                None => {
                    let meta = session::most_recent_session(&base)?;
                    eprintln!("attaching to {}", meta.name);
                    meta.name
                }
            };
            client::attach(&base, &name)
        }
        Commands::List => {
            let sessions = session::list_sessions(&base)?;
            if sessions.is_empty() {
                println!("(no sessions)");
                return Ok(());
            }
            println!(
                "{:<20} {:>8} {:<10} {:<20} SHELL",
                "NAME", "PID", "STATE", "CREATED"
            );
            for (meta, _) in sessions {
                let state = if meta.attached {
                    "attached"
                } else {
                    "detached"
                };
                let created = format_unix(meta.created_unix);
                println!(
                    "{:<20} {:>8} {:<10} {:<20} {}",
                    meta.name, meta.pid, state, created, meta.shell
                );
            }
            Ok(())
        }
        Commands::Kill { name } => {
            session::kill_session(&base, &name)?;
            println!("killed {name}");
            Ok(())
        }
    }
}

fn default_shell() -> String {
    "/bin/zsh".into()
}

fn format_unix(ts: u64) -> String {
    // Keep formatting dependency-free: show unix timestamp.
    // Good enough for list output in v1.
    ts.to_string()
}
