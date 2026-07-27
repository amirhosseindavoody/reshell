mod client;
mod history;
mod picker;
mod protocol;
mod server;
mod session;
mod ssh;
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

    /// Override ended-session archive directory.
    /// Default: `$XDG_STATE_HOME/reshell/archive` (or `$HOME/.local/state/reshell/archive`);
    /// with `--dir`, defaults to `$dir/archive`.
    #[arg(long, global = true, env = "RESHELL_ARCHIVE_DIR")]
    archive_dir: Option<PathBuf>,

    /// Daemon log path (default: `$session/daemon.log`). Also accepts `RESHELL_LOG`.
    #[arg(long, global = true, env = "RESHELL_LOG")]
    log: Option<PathBuf>,

    /// Detach key (default: Ctrl+\ ). Examples: ^\, ^a, 0x1c. Also `RESHELL_DETACH_KEY`.
    #[arg(long, global = true, env = "RESHELL_DETACH_KEY", default_value = "^\\")]
    detach_key: String,

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
    /// With no name: interactive picker (`n` create / attach).
    /// Non-TTY falls back to the most recently active session; with no sessions,
    /// creates one (TTY: prompts for name).
    #[command(visible_alias = "a")]
    Attach {
        /// Session name (omit for the interactive picker)
        #[arg(add = ArgValueCompleter::new(complete_attachable_session_name))]
        name: Option<String>,
    },
    /// Detach the client from a session (shell keeps running).
    /// Defaults to the current session when inside one, otherwise the most
    /// recently active session.
    #[command(visible_alias = "d")]
    Detach {
        /// Session name (omit for current / most recent)
        #[arg(add = ArgValueCompleter::new(complete_session_name))]
        name: Option<String>,
    },
    /// List running sessions
    #[command(visible_alias = "ls")]
    List {
        /// Machine-readable JSON (stable fields for scripts)
        #[arg(long)]
        json: bool,
        /// Include ended (archived) sessions as well as live ones
        #[arg(long)]
        all: bool,
    },
    /// Show details for a session
    #[command(visible_alias = "i")]
    Info {
        /// Session name or archive id (`name@unix`). Defaults to the current
        /// session when inside one, otherwise the most recently active live
        /// session (falls back to the newest matching archive if that live
        /// session is gone).
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
    /// Clean up session storage.
    ///
    /// Without flags: remove dead / orphan *live* session directories (after
    /// archiving their history and daemon.log). Live sessions keep running;
    /// archives are not deleted. Also runs automatically as part of `list`.
    Clean {
        /// Also purge archived ended sessions (history + daemon.log)
        #[arg(long)]
        all: bool,
    },
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
    /// SSH to a Linux host, ensure remote reshell, and attach (with reconnect)
    ///
    /// Thin wrapper: SSHes in, checks/installs a compatible remote `reshell`
    /// (`pixi global install` when needed), then creates or attaches a session
    /// daemon on the server. If the SSH link drops, waits with backoff
    /// (1s → 60s) and reconnects; press `R` to retry immediately, `Q` to quit.
    /// Clean detach (Ctrl+\ by default) exits without reconnecting.
    ///
    /// Examples:
    ///
    ///   reshell ssh myserver
    ///
    ///   reshell ssh -n demo user@host
    ///
    ///   reshell ssh -- -J bastion -p 2222 user@host
    Ssh {
        /// Remote session name (generated once if omitted; reused on reconnect)
        #[arg(long, short = 'n')]
        name: Option<String>,
        /// Shell for a newly created remote session (default: /bin/zsh)
        #[arg(long)]
        shell: Option<String>,
        /// Do not auto-install remote reshell via pixi when missing/incompatible
        #[arg(long)]
        no_install: bool,
        /// Git URL for remote `pixi global install --git` (default: GitHub repo)
        #[arg(long, default_value = ssh::DEFAULT_INSTALL_GIT)]
        install_git: String,
        /// Git branch/tag for remote install (default: main)
        #[arg(long, default_value = ssh::DEFAULT_INSTALL_REF)]
        install_ref: String,
        /// Destination (`Host` from ssh config, or `user@host`).
        /// Omit when the host is among the forwarded ssh args after `--`.
        destination: Option<String>,
        /// Extra arguments forwarded to `ssh` (place after `--` if they look like flags)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        ssh_args: Vec<String>,
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

    let custom_base = cli.dir.is_some();
    let base = match cli.dir {
        Some(d) => d,
        None => session_base_dir()?,
    };
    let archive = session::resolve_archive_dir(cli.archive_dir.as_deref(), &base, custom_base);
    let log = cli.log;
    let detach_key = parse_detach_key(&cli.detach_key)?;

    // Bare `reshell` is an alias for `reshell attach`.
    let command = cli.command.unwrap_or(Commands::Attach { name: None });

    match command {
        Commands::Ssh {
            name,
            shell,
            no_install,
            install_git,
            install_ref,
            destination,
            ssh_args,
        } => {
            // SSH mode does not use the local session dir; remote owns the daemon.
            let _ = (&base, &archive, &log);
            ssh::run(ssh::SshOpts {
                name,
                shell,
                detach_key: cli.detach_key,
                no_install,
                install_git,
                install_ref,
                destination,
                ssh_args,
                ssh_bin: None,
                expected_version: None,
            })
        }
        Commands::New {
            name,
            shell,
            detach,
        } => cmd_new(&base, &archive, name, shell, detach, log, detach_key),
        Commands::Attach { name } => cmd_attach(&base, &archive, name, log, detach_key),
        Commands::Detach { name } => cmd_detach(&base, &archive, name),
        Commands::List { json, all } => cmd_list(&base, &archive, json, all),
        Commands::Info { name, json } => cmd_info(&base, &archive, name, json),
        Commands::Rename { old_name, new_name } => {
            session::rename_session(&base, &old_name, &new_name)?;
            println!("renamed {old_name} → {new_name}");
            Ok(())
        }
        Commands::Clean { all } => {
            let n_live = session::cleanup_stale_sessions(&base, &archive)?;
            if all {
                let n_arch = session::purge_ended_sessions(&archive)?;
                if n_live == 0 && n_arch == 0 {
                    println!("(nothing to clean)");
                } else {
                    if n_live > 0 {
                        println!(
                            "cleaned {n_live} dead/orphan live session dir(s); \
                             history/logs were archived under {}",
                            archive.display()
                        );
                    }
                    if n_arch > 0 {
                        println!("purged {n_arch} ended session(s) from {}", archive.display());
                    } else if n_live > 0 {
                        println!("(no ended sessions to purge under {})", archive.display());
                    }
                }
            } else if n_live == 0 {
                println!("(nothing to clean)");
            } else {
                println!(
                    "cleaned {n_live} dead/orphan live session dir(s); \
                     history/logs kept under {}",
                    archive.display()
                );
            }
            Ok(())
        }
        Commands::Kill { name, all } => {
            if all {
                let killed = session::kill_all_sessions(&base, &archive)?;
                if killed.is_empty() {
                    println!("(no sessions)");
                } else {
                    for name in &killed {
                        println!("killed {name}");
                    }
                }
            } else {
                let name = name.expect("clap requires name unless --all");
                session::kill_session(&base, &name, &archive)?;
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

/// Tab-complete live session names for `info` / `detach` / `kill` / `rename`.
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
    let (base, archive) = completion_dirs();
    let Ok(sessions) = session::list_sessions(&base, &archive) else {
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

fn completion_dirs() -> (PathBuf, PathBuf) {
    let custom = dir_from_completion_args().is_some()
        || std::env::var("RESHELL_DIR").map(|s| !s.is_empty()).unwrap_or(false);
    let base = completion_base_dir();
    let archive_explicit = archive_dir_from_completion_args()
        .or_else(|| {
            std::env::var("RESHELL_ARCHIVE_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        });
    let archive = session::resolve_archive_dir(archive_explicit.as_deref(), &base, custom);
    (base, archive)
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
    flag_value_from_completion_args("--dir")
}

fn archive_dir_from_completion_args() -> Option<PathBuf> {
    flag_value_from_completion_args("--archive-dir")
}

fn flag_value_from_completion_args(flag: &str) -> Option<PathBuf> {
    let args: Vec<_> = std::env::args_os().collect();
    let start = args
        .iter()
        .position(|a| a == "--")
        .map(|i| i + 1)
        .unwrap_or(0);
    let words = &args[start..];
    let eq = format!("{flag}=");
    let mut i = 0;
    while i < words.len() {
        let w = words[i].to_string_lossy();
        if w == flag {
            return words.get(i + 1).map(PathBuf::from);
        }
        if let Some(rest) = w.strip_prefix(&eq) {
            return Some(PathBuf::from(rest));
        }
        i += 1;
    }
    None
}

fn cmd_new(
    base: &Path,
    archive: &Path,
    name: Option<String>,
    shell: Option<String>,
    detach: bool,
    log: Option<PathBuf>,
    detach_key: u8,
) -> Result<()> {
    let _ = session::cleanup_stale_sessions(base, archive)?;
    let name = match name {
        Some(n) => n,
        None => allocate_session_name(base)?,
    };
    let shell = shell.unwrap_or_else(default_shell);
    server::create_session(server::NewSessionOpts {
        name: name.clone(),
        shell,
        base: base.to_path_buf(),
        archive: archive.to_path_buf(),
        log_path: log,
    })?;
    if detach {
        println!("{name}");
        Ok(())
    } else {
        // Name goes to stderr so it does not collide with the TTY session.
        eprintln!("{name}");
        // If this process is already inside a session, leave it and join the
        // new one via the outer attach client — never nest a second client.
        join_session(base, archive, &name, detach_key)
    }
}

fn cmd_attach(
    base: &Path,
    archive: &Path,
    name: Option<String>,
    log: Option<PathBuf>,
    detach_key: u8,
) -> Result<()> {
    match name {
        Some(n) => join_session(base, archive, &n, detach_key),
        None => {
            let mut sessions = session::list_sessions(base, archive)?;

            let stdin_fd = std::io::stdin().as_raw_fd();
            let is_tty = nix::unistd::isatty(stdin_fd).unwrap_or(false);

            if sessions.is_empty() {
                if is_tty {
                    // Prompt for a name (editable suggested default), then create.
                    match picker::prompt_new_session_name(base)? {
                        Some(n) => {
                            return cmd_new(base, archive, Some(n), None, false, log, detach_key);
                        }
                        None => anyhow::bail!("cancelled"),
                    }
                }
                // Non-TTY: same as `reshell new` (auto name).
                return cmd_new(base, archive, None, None, false, log, detach_key);
            }

            if !is_tty {
                // Scripts / pipes: keep the historical most-recent default.
                let meta = session::most_recent_session(base, archive)?;
                return join_session(base, archive, &meta.name, detach_key);
            }

            // Detached (attachable) first by activity, then attached (gray).
            sessions.sort_by(|a, b| match (a.0.attached, b.0.attached) {
                (false, true) => std::cmp::Ordering::Less,
                (true, false) => std::cmp::Ordering::Greater,
                _ => session::session_activity(&b.0)
                    .cmp(&session::session_activity(&a.0))
                    .then_with(|| a.0.name.cmp(&b.0.name)),
            });

            let current_name = session::current_session(base, archive)?.map(|m| m.name);

            let rows: Vec<picker::SessionRow> = sessions
                .iter()
                .map(|(meta, paths)| {
                    let state = if meta.attached {
                        "attached"
                    } else {
                        "detached"
                    };
                    let history = history::current_history_file(paths)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(none)".into());
                    picker::SessionRow {
                        name: meta.name.clone(),
                        attached: meta.attached,
                        current: current_name.as_deref() == Some(meta.name.as_str()),
                        state: state.into(),
                        created: format_time_human(meta.created_unix),
                        last_active: format_time_human(session::session_activity(meta)),
                        shell: meta.shell.clone(),
                        history,
                    }
                })
                .collect();

            match picker::pick_session(base, archive, &rows)? {
                // `cmd_new` / `join_session` leave the current session when inside one.
                picker::PickAction::CreateNew { name } => {
                    cmd_new(base, archive, Some(name), None, false, log, detach_key)
                }
                picker::PickAction::Attach(n) => join_session(base, archive, &n, detach_key),
                picker::PickAction::AttachAfterDetach(n) => {
                    // Confirmed in the picker: free the other terminal first, then
                    // join (still exclusive — only one attach at a time).
                    eprintln!("detaching {n}");
                    session::request_detach(base, &n)?;
                    join_session(base, archive, &n, detach_key)
                }
                picker::PickAction::Cancelled => {
                    anyhow::bail!("cancelled");
                }
            }
        }
    }
}

fn cmd_detach(base: &Path, archive: &Path, name: Option<String>) -> Result<()> {
    let name = resolve_session_name(base, archive, name)?;
    let paths = session::SessionPaths::for_name(base, &name);
    if !paths.meta.exists() {
        anyhow::bail!("session '{name}' not found");
    }
    if !session::is_attached(&paths) {
        println!("session '{name}' is already detached");
        return Ok(());
    }
    session::request_detach(base, &name)?;
    println!("detached {name}");
    Ok(())
}

/// Join `target`, never nesting on top of a session this process is already in.
///
/// Invariant: if `current_session` is some other live session, ask its outer
/// attach client to detach that session and attach to `target` instead of
/// calling `client::attach` from this process. Same-session is a no-op.
fn join_session(base: &Path, archive: &Path, target: &str, detach_key: u8) -> Result<()> {
    if let Some(cur) = session::current_session(base, archive)? {
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

fn cmd_list(base: &Path, archive: &Path, json: bool, all: bool) -> Result<()> {
    let sessions = session::list_sessions(base, archive)?;
    let ended = if all {
        session::list_ended_sessions(archive)?
    } else {
        Vec::new()
    };

    if json {
        let mut rows: Vec<SessionJson> = sessions
            .iter()
            .map(|(meta, paths)| SessionJson::from_session(meta, paths, None))
            .collect();
        if all {
            rows.extend(ended.iter().map(SessionJson::from_ended));
        }
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if sessions.is_empty() && ended.is_empty() {
        if all {
            println!("(no live or ended sessions)");
        } else {
            println!("(no sessions)");
        }
        return Ok(());
    }

    if all {
        println!(
            "{:<20} {:>8} {:<10} {:<16} {:<16} {}",
            "NAME", "PID", "STATE", "CREATED", "LAST ACTIVE", "SHELL/REASON"
        );
    } else {
        println!(
            "{:<20} {:>8} {:<10} {:<16} {:<16} SHELL",
            "NAME", "PID", "STATE", "CREATED", "LAST ACTIVE"
        );
    }
    for (meta, _) in &sessions {
        let state = if meta.attached {
            "attached"
        } else {
            "detached"
        };
        let created = format_time_human(meta.created_unix);
        let last_active = format_time_human(session::session_activity(meta));
        println!(
            "{:<20} {:>8} {:<10} {:<16} {:<16} {}",
            meta.name, meta.pid, state, created, last_active, meta.shell
        );
    }
    for e in &ended {
        let reason = e.meta.end_reason.as_deref().unwrap_or("-");
        let created = format_time_human(e.meta.created_unix);
        let last = format_time_human(
            e.meta
                .ended_unix
                .unwrap_or_else(|| session::session_activity(&e.meta)),
        );
        let detail = format!("{reason} ({})", e.archive_id);
        println!(
            "{:<20} {:>8} {:<10} {:<16} {:<16} {}",
            e.meta.name, "-", "ended", created, last, detail
        );
    }
    Ok(())
}

fn resolve_session_name(base: &Path, archive: &Path, name: Option<String>) -> Result<String> {
    match name {
        Some(n) => Ok(n),
        None => match session::current_session(base, archive)? {
            Some(meta) => Ok(meta.name),
            None => Ok(session::most_recent_session(base, archive)?.name),
        },
    }
}

fn cmd_info(base: &Path, archive: &Path, name: Option<String>, json: bool) -> Result<()> {
    // Prefer live; if missing/dead after cleanup, fall back to newest archive.
    // Archive ids (`name@unix`) are accepted via the ended-session lookup.
    let resolved = match name {
        Some(ref n) => Some(n.clone()),
        None => match session::current_session(base, archive)? {
            Some(meta) => Some(meta.name),
            None => session::most_recent_session(base, archive)
                .ok()
                .map(|m| m.name),
        },
    };

    if let Some(ref n) = resolved {
        // Exact archive id first (contains `@`, not a valid live name).
        if n.contains('@') {
            if let Ok(e) = session::find_ended_session(archive, n) {
                return print_session_info(&e.meta, &e.paths, json, Some(&e.archive_id));
            }
        }
        match session::session_info(base, n, archive) {
            Ok((meta, paths)) => return print_session_info(&meta, &paths, json, None),
            Err(_) => {
                if let Ok(e) = session::find_ended_session(archive, n) {
                    return print_session_info(&e.meta, &e.paths, json, Some(&e.archive_id));
                }
            }
        }
    } else if let Ok(e) = session::list_ended_sessions(archive).map(|v| v.into_iter().next()) {
        if let Some(e) = e {
            return print_session_info(&e.meta, &e.paths, json, Some(&e.archive_id));
        }
    }

    match resolved {
        Some(n) => anyhow::bail!("session '{n}' not found (live or ended)"),
        None => anyhow::bail!("no sessions found"),
    }
}

fn print_session_info(
    meta: &session::SessionMeta,
    paths: &session::SessionPaths,
    json: bool,
    archive_id: Option<&str>,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&SessionJson::from_session(meta, paths, archive_id))?
        );
        return Ok(());
    }
    let state = if meta.ended_unix.is_some() {
        "ended"
    } else if meta.attached {
        "attached"
    } else {
        "detached"
    };
    println!("name:        {}", meta.name);
    if let Some(id) = archive_id {
        println!("archive_id:  {id}");
    }
    println!("pid:         {}", meta.pid);
    println!("state:       {state}");
    if let Some(reason) = meta.end_reason.as_deref() {
        println!("end_reason:  {reason}");
    }
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
    if let Some(ended) = meta.ended_unix {
        println!(
            "ended:       {} ({})",
            format_time_human(ended),
            ended
        );
    }
    println!("dir:         {}", paths.dir.display());
    if meta.ended_unix.is_none() {
        println!("socket:      {}", paths.socket.display());
        println!("meta:        {}", paths.meta.display());
        println!("attach_lock: {}", paths.attach_lock.display());
    } else {
        println!("meta:        {}", paths.meta.display());
    }
    println!("daemon_log:  {}", paths.daemon_log.display());
    println!("history_dir: {}", paths.history_dir().display());
    let history_files = history::list_history_files(paths);
    if history_files.is_empty() {
        println!("history:     (none yet)");
    } else {
        let last_i = history_files.len() - 1;
        for (i, path) in history_files.iter().enumerate() {
            let label = if i == 0 { "history:" } else { "         " };
            if i == last_i {
                println!("{label}     {}  (current)", path.display());
            } else {
                println!("{label}     {}", path.display());
            }
        }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    ended_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    dir: String,
    socket: String,
    meta: String,
    attach_lock: String,
    daemon_log: String,
    history_dir: String,
    history_files: Vec<String>,
}

impl SessionJson {
    fn from_session(
        meta: &session::SessionMeta,
        paths: &session::SessionPaths,
        archive_id: Option<&str>,
    ) -> Self {
        let history_files = history::list_history_files(paths);
        let state = if meta.ended_unix.is_some() {
            Some("ended".into())
        } else if meta.attached {
            Some("attached".into())
        } else {
            Some("detached".into())
        };
        Self {
            name: meta.name.clone(),
            pid: meta.pid,
            shell: meta.shell.clone(),
            attached: meta.attached,
            created_unix: meta.created_unix,
            last_active_unix: meta.last_active_unix,
            ended_unix: meta.ended_unix,
            end_reason: meta.end_reason.clone(),
            archive_id: archive_id.map(|s| s.to_string()),
            state,
            dir: paths.dir.display().to_string(),
            socket: paths.socket.display().to_string(),
            meta: paths.meta.display().to_string(),
            attach_lock: paths.attach_lock.display().to_string(),
            daemon_log: paths.daemon_log.display().to_string(),
            history_dir: paths.history_dir().display().to_string(),
            history_files: history_files
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
        }
    }

    fn from_ended(e: &session::EndedSession) -> Self {
        Self::from_session(&e.meta, &e.paths, Some(&e.archive_id))
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
