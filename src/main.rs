mod client;
mod protocol;
mod server;
mod session;
mod termstate;

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};

use session::{generate_session_name, session_base_dir};

#[derive(Debug, Parser)]
#[command(
    name = "reshell",
    version,
    about = "Keep shells alive across SSH disconnects with explicit attach/detach sessions",
    subcommand_required = false
)]
struct Cli {
    /// Override session storage directory (default: $XDG_RUNTIME_DIR/reshell)
    #[arg(long, global = true, env = "RESHELL_DIR")]
    dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
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
    /// Attach to an existing session (most recently active if name omitted).
    /// With no sessions, creates a new one (same as `new`).
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

    // Bare `reshell` is an alias for `reshell attach`.
    let command = cli.command.unwrap_or(Commands::Attach { name: None });

    match command {
        Commands::New {
            name,
            shell,
            detach,
        } => cmd_new(&base, name, shell, detach),
        Commands::Attach { name } => cmd_attach(&base, name),
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

fn cmd_new(base: &Path, name: Option<String>, shell: Option<String>, detach: bool) -> Result<()> {
    let name = name.unwrap_or_else(generate_session_name);
    let shell = shell.unwrap_or_else(default_shell);
    server::create_session(server::NewSessionOpts {
        name: name.clone(),
        shell,
        base: base.to_path_buf(),
    })?;
    if detach {
        println!("{name}");
        Ok(())
    } else {
        // Name goes to stderr so it does not collide with the TTY session.
        eprintln!("{name}");
        client::attach(base, &name)
    }
}

fn cmd_attach(base: &Path, name: Option<String>) -> Result<()> {
    match name {
        Some(n) => client::attach(base, &n),
        None => {
            let sessions = session::list_sessions(base)?;
            if sessions.is_empty() {
                // No live sessions — same as `reshell new`.
                return cmd_new(base, None, None, false);
            }
            let meta = session::most_recent_session(base)?;
            eprintln!("attaching to {}", meta.name);
            client::attach(base, &meta.name)
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
