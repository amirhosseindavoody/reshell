mod client;
mod context;
mod picker;
mod protocol;
mod scrollback;
mod server;
mod session;
mod termstate;
mod vscode_si;

use std::ffi::OsStr;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use clap_complete::{CompleteEnv, Shell};
use serde::Serialize;

use protocol::parse_detach_key;
use scrollback::parse_scrollback_size;
use session::{allocate_session_name, now_unix, session_base_dir};

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

    /// Daemon log path (default: `$session/daemon.log`). Also accepts `RESHELL_LOG`.
    #[arg(long, global = true, env = "RESHELL_LOG")]
    log: Option<PathBuf>,

    /// Detach key (default: Ctrl+\ ). Examples: ^\, ^a, 0x1c. Also `RESHELL_DETACH_KEY`.
    #[arg(long, global = true, env = "RESHELL_DETACH_KEY", default_value = "^\\")]
    detach_key: String,

    /// Detached PTY bytes to keep and replay on attach (0=off). Examples: 1M, 512K.
    /// Applied when creating a session (`new` / picker "Create new"). Also `RESHELL_SCROLLBACK`.
    #[arg(long, global = true, env = "RESHELL_SCROLLBACK", default_value = "0")]
    scrollback: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create a new session and attach to it
    #[command(visible_alias = "n")]
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
    /// Attach to an existing session.
    /// With no name: interactive picker (create new with name prompt / attach).
    /// Non-TTY falls back to the most recently active session; with no sessions,
    /// creates one (TTY: prompts for name).
    #[command(visible_alias = "a")]
    Attach {
        /// Session name (omit for the interactive picker)
        #[arg(add = ArgValueCompleter::new(complete_attachable_session_name))]
        name: Option<String>,
    },
    /// List running sessions
    #[command(visible_alias = "ls")]
    List {
        /// Machine-readable JSON (stable fields for scripts)
        #[arg(long)]
        json: bool,
    },
    /// Show details for a session
    #[command(visible_alias = "i")]
    Info {
        /// Session name (defaults to the current session when inside one,
        /// otherwise the most recently active session)
        #[arg(add = ArgValueCompleter::new(complete_session_name))]
        name: Option<String>,
        /// Machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Show recent shell context (last command + trailing output)
    #[command(visible_alias = "c")]
    Context {
        /// Session name (defaults to the current session when inside one,
        /// otherwise the most recently active session)
        #[arg(add = ArgValueCompleter::new(complete_session_name))]
        name: Option<String>,
        /// Machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Rename a live session
    #[command(visible_alias = "r")]
    Rename {
        /// Current session name
        #[arg(add = ArgValueCompleter::new(complete_session_name))]
        old_name: String,
        /// New session name
        new_name: String,
    },
    /// Remove dead-session leftovers (also done automatically by `list`)
    Clean,
    /// Terminate a session and its shell
    #[command(visible_alias = "k")]
    Kill {
        /// Session name (required unless `--all`)
        #[arg(
            add = ArgValueCompleter::new(complete_session_name),
            required_unless_present = "all"
        )]
        name: Option<String>,
        /// Kill all live sessions
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
    /// Print shell completion script to stdout
    Completion {
        /// Shell to generate completions for
        shell: Shell,
    },
}

fn main() {
    // Dynamic completions (session names, etc.) — must run before any stdout.
    // Use a command tree with flags marked hidden so tab completion suggests
    // subcommands / session names only; flags stay in `--help`.
    CompleteEnv::with_factory(cli_for_completion)
        .bin("reshell")
        .complete();

    if let Err(e) = run() {
        eprintln!("reshell: {e:#}");
        std::process::exit(1);
    }
}

/// Clap command used only for dynamic completion: hide option flags so they
/// are not offered on Tab (still documented via the real CLI's `--help`), and
/// present subcommands as long names with short aliases in the description
/// (e.g. value `new`, help `(n)` → shells show `new (n)`).
fn cli_for_completion() -> clap::Command {
    rewrite_subcommands_for_completion(hide_option_flags(Cli::command()))
}

fn hide_option_flags(cmd: clap::Command) -> clap::Command {
    cmd.mut_args(|arg| {
        if arg.is_positional() {
            arg
        } else {
            arg.hide(true)
        }
    })
    .disable_help_flag(true)
    .disable_version_flag(true)
    .mut_subcommands(hide_option_flags)
}

/// Prefer long subcommand names in Tab completion, with short aliases shown in
/// the candidate help (zsh/fish descriptions). Rebuilds each subcommand so
/// visible aliases become hidden — otherwise clap_complete's id-dedup keeps
/// whichever of `new`/`n` sorts first alphabetically (often just `n`).
fn rewrite_subcommands_for_completion(cmd: clap::Command) -> clap::Command {
    cmd.mut_subcommands(rewrite_subcommand_for_completion)
}

fn rewrite_subcommand_for_completion(sc: clap::Command) -> clap::Command {
    let visible: Vec<String> = sc.get_visible_aliases().map(|s| s.to_string()).collect();
    if visible.is_empty() {
        return sc.mut_subcommands(rewrite_subcommand_for_completion);
    }

    let name = sc.get_name().to_string();
    let alias_note = visible.join(", ");
    let about = match sc.get_about().map(|a| a.to_string()) {
        Some(a) if !a.is_empty() => format!("({alias_note}) {a}"),
        _ => format!("({alias_note})"),
    };
    let all_aliases: Vec<String> = sc.get_all_aliases().map(|s| s.to_string()).collect();

    // clap::builder::Str accepts &'static str; completion runs in a short-lived
    // COMPLETE= process, so leaking a few small strings is fine.
    let name: &'static str = Box::leak(name.into_boxed_str());
    let all_aliases: Vec<&'static str> = all_aliases
        .into_iter()
        .map(|s| &*Box::leak(s.into_boxed_str()))
        .collect();

    let mut out = clap::Command::new(name)
        .about(about)
        .aliases(all_aliases)
        .display_order(sc.get_display_order());
    if let Some(long) = sc.get_long_about() {
        out = out.long_about(long.clone());
    }
    if sc.is_hide_set() {
        out = out.hide(true);
    }
    for arg in sc.get_arguments() {
        out = out.arg(arg.clone());
    }
    for sub in sc.get_subcommands() {
        out = out.subcommand(rewrite_subcommand_for_completion(sub.clone()));
    }
    hide_option_flags(out)
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    // Registration script that calls back into this binary for dynamic values.
    if let Some(Commands::Completion { shell }) = cli.command {
        return print_completion_registration(shell);
    }

    let base = match cli.dir {
        Some(d) => d,
        None => session_base_dir()?,
    };
    let log = cli.log;
    let detach_key = parse_detach_key(&cli.detach_key)?;
    let scrollback = parse_scrollback_size(&cli.scrollback)?;

    // Bare `reshell` is an alias for `reshell attach`.
    let command = cli.command.unwrap_or(Commands::Attach { name: None });

    match command {
        Commands::New {
            name,
            shell,
            detach,
        } => cmd_new(&base, name, shell, detach, log, detach_key, scrollback),
        Commands::Attach { name } => cmd_attach(&base, name, log, detach_key, scrollback),
        Commands::List { json } => cmd_list(&base, json),
        Commands::Info { name, json } => cmd_info(&base, name, json),
        Commands::Context { name, json } => cmd_context(&base, name, json),
        Commands::Rename { old_name, new_name } => {
            session::rename_session(&base, &old_name, &new_name)?;
            println!("renamed {old_name} → {new_name}");
            Ok(())
        }
        Commands::Clean => {
            let n = session::cleanup_stale_sessions(&base)?;
            if n == 0 {
                println!("(nothing to clean)");
            } else {
                println!("removed {n} stale session(s)");
            }
            Ok(())
        }
        Commands::Kill { name, all } => {
            if all {
                let killed = session::kill_all_sessions(&base)?;
                if killed.is_empty() {
                    println!("(no sessions)");
                } else {
                    for name in &killed {
                        println!("killed {name}");
                    }
                }
            } else {
                let name = name.expect("clap requires name unless --all");
                session::kill_session(&base, &name)?;
                println!("killed {name}");
            }
            Ok(())
        }
        Commands::Completion { .. } => unreachable!("handled above"),
    }
}

/// Print the dynamic completion registration script for `shell`.
fn print_completion_registration(shell: Shell) -> Result<()> {
    let argv0 = std::env::args_os()
        .next()
        .unwrap_or_else(|| "reshell".into());
    // SAFETY: only set during CLI init before other threads; CompleteEnv clears it.
    unsafe {
        std::env::set_var("COMPLETE", shell.to_string());
    }
    let current_dir = std::env::current_dir().ok();
    let done = CompleteEnv::with_factory(cli_for_completion)
        .bin("reshell")
        .try_complete([argv0], current_dir.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !done {
        anyhow::bail!("failed to generate {shell} completion script");
    }
    Ok(())
}

/// Tab-complete live session names for `info` / `context` / `kill` / `rename`.
fn complete_session_name(current: &OsStr) -> Vec<CompletionCandidate> {
    complete_sessions(current, /*attachable_only=*/ false)
}

/// Tab-complete sessions that can be attached (live and not already attached).
fn complete_attachable_session_name(current: &OsStr) -> Vec<CompletionCandidate> {
    complete_sessions(current, /*attachable_only=*/ true)
}

fn complete_sessions(current: &OsStr, attachable_only: bool) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let base = completion_base_dir();
    let Ok(sessions) = session::list_sessions(&base) else {
        return Vec::new();
    };
    sessions
        .into_iter()
        .filter(|(meta, paths)| {
            meta.name.starts_with(current)
                && (!attachable_only || !session::is_attached(paths))
        })
        .map(|(meta, _)| CompletionCandidate::new(meta.name))
        .collect()
}

fn completion_base_dir() -> PathBuf {
    if let Some(dir) = dir_from_completion_args() {
        return dir;
    }
    if let Ok(dir) = std::env::var("RESHELL_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    session_base_dir().unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Parse `--dir` from the shell words passed to the dynamic completer.
fn dir_from_completion_args() -> Option<PathBuf> {
    let args: Vec<_> = std::env::args_os().collect();
    let start = args
        .iter()
        .position(|a| a == "--")
        .map(|i| i + 1)
        .unwrap_or(0);
    let words = &args[start..];
    let mut i = 0;
    while i < words.len() {
        let w = words[i].to_string_lossy();
        if w == "--dir" {
            return words.get(i + 1).map(PathBuf::from);
        }
        if let Some(rest) = w.strip_prefix("--dir=") {
            return Some(PathBuf::from(rest));
        }
        i += 1;
    }
    None
}

fn cmd_new(
    base: &Path,
    name: Option<String>,
    shell: Option<String>,
    detach: bool,
    log: Option<PathBuf>,
    detach_key: u8,
    scrollback: usize,
) -> Result<()> {
    let _ = session::cleanup_stale_sessions(base)?;
    let name = match name {
        Some(n) => n,
        None => allocate_session_name(base)?,
    };
    let shell = shell.unwrap_or_else(default_shell);
    server::create_session(server::NewSessionOpts {
        name: name.clone(),
        shell,
        base: base.to_path_buf(),
        log_path: log,
        scrollback,
    })?;
    if detach {
        println!("{name}");
        Ok(())
    } else {
        // Name goes to stderr so it does not collide with the TTY session.
        eprintln!("{name}");
        // If this process is already inside a session, leave it and join the
        // new one via the outer attach client — never nest a second client.
        join_session(base, &name, detach_key)
    }
}

fn cmd_attach(
    base: &Path,
    name: Option<String>,
    log: Option<PathBuf>,
    detach_key: u8,
    scrollback: usize,
) -> Result<()> {
    match name {
        Some(n) => join_session(base, &n, detach_key),
        None => {
            let mut sessions = session::list_sessions(base)?;

            let stdin_fd = std::io::stdin().as_raw_fd();
            let is_tty = nix::unistd::isatty(stdin_fd).unwrap_or(false);

            if sessions.is_empty() {
                if is_tty {
                    // Prompt for a name (editable suggested default), then create.
                    match picker::prompt_new_session_name(base)? {
                        Some(n) => {
                            return cmd_new(
                                base,
                                Some(n),
                                None,
                                false,
                                log,
                                detach_key,
                                scrollback,
                            );
                        }
                        None => anyhow::bail!("cancelled"),
                    }
                }
                // Non-TTY: same as `reshell new` (auto name).
                return cmd_new(base, None, None, false, log, detach_key, scrollback);
            }

            if !is_tty {
                // Scripts / pipes: keep the historical most-recent default.
                let meta = session::most_recent_session(base)?;
                return join_session(base, &meta.name, detach_key);
            }

            // Detached (attachable) first by activity, then attached (gray).
            sessions.sort_by(|a, b| match (a.0.attached, b.0.attached) {
                (false, true) => std::cmp::Ordering::Less,
                (true, false) => std::cmp::Ordering::Greater,
                _ => session::session_activity(&b.0)
                    .cmp(&session::session_activity(&a.0))
                    .then_with(|| a.0.name.cmp(&b.0.name)),
            });

            let current_name = session::current_session(base)?
                .map(|m| m.name);

            let rows: Vec<picker::SessionRow> = sessions
                .iter()
                .map(|(meta, _)| {
                    let state = if meta.attached {
                        "attached"
                    } else {
                        "detached"
                    };
                    picker::SessionRow {
                        name: meta.name.clone(),
                        attached: meta.attached,
                        current: current_name.as_deref() == Some(meta.name.as_str()),
                        state: state.into(),
                        created: format_time_human(meta.created_unix),
                        last_active: format_time_human(session::session_activity(meta)),
                        shell: meta.shell.clone(),
                    }
                })
                .collect();

            match picker::pick_session(base, &rows)? {
                // `cmd_new` / `join_session` leave the current session when inside one.
                picker::PickAction::CreateNew { name } => {
                    cmd_new(base, Some(name), None, false, log, detach_key, scrollback)
                }
                picker::PickAction::Attach(n) => join_session(base, &n, detach_key),
                picker::PickAction::Cancelled => {
                    anyhow::bail!("cancelled");
                }
            }
        }
    }
}

/// Join `target`, never nesting on top of a session this process is already in.
///
/// Invariant: if `current_session` is some other live session, ask its outer
/// attach client to detach that session and attach to `target` instead of
/// calling `client::attach` from this process. Same-session is a no-op.
fn join_session(base: &Path, target: &str, detach_key: u8) -> Result<()> {
    if let Some(cur) = session::current_session(base)? {
        if cur.name == target {
            eprintln!("already in session '{target}'");
            return Ok(());
        }
        eprintln!("switching from {} to {target}", cur.name);
        return session::request_attach_switch(base, &cur.name, target);
    }
    eprintln!("attaching to {target}");
    client::attach(base, target, detach_key)
}

fn cmd_list(base: &Path, json: bool) -> Result<()> {
    let sessions = session::list_sessions(base)?;
    if json {
        let rows: Vec<SessionJson> = sessions
            .iter()
            .map(|(meta, paths)| SessionJson::from_session(meta, paths))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if sessions.is_empty() {
        println!("(no sessions)");
        return Ok(());
    }
    println!(
        "{:<20} {:>8} {:<10} {:<16} {:<16} SHELL",
        "NAME", "PID", "STATE", "CREATED", "LAST ACTIVE"
    );
    for (meta, _) in sessions {
        let state = if meta.attached {
            "attached"
        } else {
            "detached"
        };
        let created = format_time_human(meta.created_unix);
        let last_active = format_time_human(session::session_activity(&meta));
        println!(
            "{:<20} {:>8} {:<10} {:<16} {:<16} {}",
            meta.name, meta.pid, state, created, last_active, meta.shell
        );
    }
    Ok(())
}

fn resolve_session_name(base: &Path, name: Option<String>) -> Result<String> {
    match name {
        Some(n) => Ok(n),
        None => match session::current_session(base)? {
            Some(meta) => Ok(meta.name),
            None => Ok(session::most_recent_session(base)?.name),
        },
    }
}

fn cmd_info(base: &Path, name: Option<String>, json: bool) -> Result<()> {
    let name = resolve_session_name(base, name)?;
    let (meta, paths) = session::session_info(base, &name)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&SessionJson::from_session(&meta, &paths))?
        );
        return Ok(());
    }
    let state = if meta.attached {
        "attached"
    } else {
        "detached"
    };
    println!("name:        {}", meta.name);
    println!("pid:         {}", meta.pid);
    println!("state:       {state}");
    println!("shell:       {}", meta.shell);
    println!(
        "created:     {} ({})",
        format_time_human(meta.created_unix),
        meta.created_unix
    );
    let last = if meta.last_active_unix > 0 {
        meta.last_active_unix
    } else {
        meta.created_unix
    };
    println!(
        "last_active: {} ({})",
        format_time_human(last),
        last
    );
    println!("dir:         {}", paths.dir.display());
    println!("socket:      {}", paths.socket.display());
    println!("meta:        {}", paths.meta.display());
    println!("attach_lock: {}", paths.attach_lock.display());
    println!("daemon_log:  {}", paths.daemon_log.display());
    Ok(())
}

fn cmd_context(base: &Path, name: Option<String>, json: bool) -> Result<()> {
    let name = resolve_session_name(base, name)?;
    let snap = client::fetch_context(base, &name)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snap)?);
        return Ok(());
    }
    println!("session: {}", snap.name);
    match &snap.last_command {
        Some(cmd) => match snap.last_exit_code {
            Some(code) => println!("last_command: {cmd}  (exit {code})"),
            None => println!("last_command: {cmd}"),
        },
        None => println!("last_command: (unknown)"),
    }
    if snap.alt_screen {
        println!("note: session is in a full-screen app; showing shell history from before it");
    }
    println!("--- output (last {} lines) ---", snap.lines.len());
    for line in &snap.lines {
        println!("{line}");
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct SessionJson {
    name: String,
    pid: i32,
    shell: String,
    attached: bool,
    created_unix: u64,
    last_active_unix: u64,
    dir: String,
    socket: String,
    meta: String,
    attach_lock: String,
    daemon_log: String,
}

impl SessionJson {
    fn from_session(meta: &session::SessionMeta, paths: &session::SessionPaths) -> Self {
        Self {
            name: meta.name.clone(),
            pid: meta.pid,
            shell: meta.shell.clone(),
            attached: meta.attached,
            created_unix: meta.created_unix,
            last_active_unix: meta.last_active_unix,
            dir: paths.dir.display().to_string(),
            socket: paths.socket.display().to_string(),
            meta: paths.meta.display().to_string(),
            attach_lock: paths.attach_lock.display().to_string(),
            daemon_log: paths.daemon_log.display().to_string(),
        }
    }
}

fn default_shell() -> String {
    "/bin/zsh".into()
}

/// Relative time for human list/info output (no extra time deps).
fn format_time_human(ts: u64) -> String {
    if ts == 0 {
        return "-".into();
    }
    let now = now_unix();
    if ts > now {
        return "in the future".into();
    }
    let ago = now - ts;
    if ago < 60 {
        format!("{ago}s ago")
    } else if ago < 3600 {
        format!("{}m ago", ago / 60)
    } else if ago < 86400 {
        format!("{}h ago", ago / 3600)
    } else if ago < 86400 * 30 {
        format!("{}d ago", ago / 86400)
    } else {
        format!("{}d ago", ago / 86400)
    }
}
