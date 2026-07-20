//! VS Code / Cursor integrated-terminal shell integration helpers.
//!
//! Sticky scroll and command decorations key off OSC 633 sequences from the
//! shell. When the user runs `reshell`, the *outer* shell marks that as the
//! current command and never sees a finish marker — so sticky scroll sticks on
//! `reshell` forever. We close that outer command on attach, and when creating
//! a session inside VS Code we inject the real shell-integration script so the
//! *inner* shell emits per-command markers that pass through our PTY pipe.

use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// OSC 633 ; D — mark the current command finished (no exit code).
///
/// Written to the local TTY on attach so VS Code releases sticky scroll from
/// the outer `reshell` / `reshell attach` invocation.
pub const OSC_633_COMMAND_FINISHED: &[u8] = b"\x1b]633;D\x07";

/// True when the current process looks like it is inside VS Code or Cursor's
/// integrated terminal.
pub fn running_in_vscode_terminal() -> bool {
    match std::env::var("TERM_PROGRAM") {
        Ok(v) if v == "vscode" || v.eq_ignore_ascii_case("cursor") => true,
        _ => std::env::var_os("VSCODE_IPC_HOOK_CLI").is_some()
            || std::env::var_os("CURSOR_TRACE_ID").is_some(),
    }
}

/// How to exec the session shell so VS Code shell integration is active.
#[derive(Debug)]
pub struct ShellLaunch {
    pub program: CString,
    pub argv: Vec<CString>,
}

/// Build argv/env for the session shell, injecting VS Code shell integration
/// when we are inside the integrated terminal and can locate the script.
///
/// Sets `VSCODE_INJECTION=1` (and zsh `ZDOTDIR` / `USER_ZDOTDIR`) as needed.
/// Always ensures `TERM_PROGRAM=vscode` when we detect a VS Code/Cursor host so
/// manual `[[ "$TERM_PROGRAM" == "vscode" ]]` installs in user rc files work.
pub fn prepare_shell_launch(shell_path: &str, session_dir: &Path) -> Result<ShellLaunch> {
    let program = CString::new(shell_path).context("shell path contains NUL")?;
    let basename = Path::new(shell_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("sh");
    let argv0 = CString::new(basename).unwrap();

    if !running_in_vscode_terminal() {
        return Ok(ShellLaunch {
            program,
            argv: vec![argv0],
        });
    }

    // So user rc scripts that gate on TERM_PROGRAM keep working even if the
    // parent unset it somehow; VS Code itself always sets this.
    if std::env::var_os("TERM_PROGRAM").is_none() {
        std::env::set_var("TERM_PROGRAM", "vscode");
    }

    // The outer integrated shell exports VSCODE_SHELL_INTEGRATION=1; the
    // injected script bails out if that is already set. Clear it so the
    // session shell actually installs prompt hooks.
    std::env::remove_var("VSCODE_SHELL_INTEGRATION");

    let kind = shell_kind(basename);
    let script = locate_shell_integration_script(kind);

    match (kind, script) {
        (ShellKind::Bash, Some(script)) => {
            std::env::set_var("VSCODE_INJECTION", "1");
            let flag = CString::new("--init-file").unwrap();
            let script_c = CString::new(script.to_string_lossy().as_ref())
                .context("shell integration path contains NUL")?;
            Ok(ShellLaunch {
                program,
                argv: vec![argv0, flag, script_c],
            })
        }
        (ShellKind::Zsh, Some(script)) => {
            let zdotdir = setup_zsh_zdotdir(session_dir, &script)?;
            let user_zdot = std::env::var_os("ZDOTDIR")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("/"));
            std::env::set_var("VSCODE_INJECTION", "1");
            std::env::set_var("USER_ZDOTDIR", &user_zdot);
            std::env::set_var("ZDOTDIR", &zdotdir);
            let interactive = CString::new("-i").unwrap();
            Ok(ShellLaunch {
                program,
                argv: vec![argv0, interactive],
            })
        }
        _ => Ok(ShellLaunch {
            program,
            argv: vec![argv0],
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Bash,
    Zsh,
    Other,
}

fn shell_kind(basename: &str) -> ShellKind {
    if basename == "bash" || basename.starts_with("bash-") {
        ShellKind::Bash
    } else if basename == "zsh" || basename.starts_with("zsh-") {
        ShellKind::Zsh
    } else {
        ShellKind::Other
    }
}

fn locate_shell_integration_script(kind: ShellKind) -> Option<PathBuf> {
    let arg = match kind {
        ShellKind::Bash => "bash",
        ShellKind::Zsh => "zsh",
        ShellKind::Other => return None,
    };

    for bin in ["code", "cursor", "code-insiders"] {
        if let Ok(out) = Command::new(bin)
            .args(["--locate-shell-integration-path", arg])
            .output()
        {
            if out.status.success() {
                let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !path.is_empty() {
                    let p = PathBuf::from(&path);
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }

    // Fall back to common install layouts when the CLI is not on PATH.
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let mut candidates = Vec::new();
    match kind {
        ShellKind::Bash => {
            candidates.push(home.join(
                ".vscode-server/bin/*/out/vs/workbench/contrib/terminal/common/scripts/shellIntegration-bash.sh",
            ));
        }
        ShellKind::Zsh => {
            candidates.push(home.join(
                ".vscode-server/bin/*/out/vs/workbench/contrib/terminal/common/scripts/shellIntegration-rc.zsh",
            ));
        }
        ShellKind::Other => {}
    }
    // Glob manually for vscode-server hashes.
    let server_bin = home.join(".vscode-server/bin");
    if let Ok(entries) = fs::read_dir(&server_bin) {
        for entry in entries.flatten() {
            let scripts = entry
                .path()
                .join("out/vs/workbench/contrib/terminal/common/scripts");
            let name = match kind {
                ShellKind::Bash => "shellIntegration-bash.sh",
                ShellKind::Zsh => "shellIntegration-rc.zsh",
                ShellKind::Other => continue,
            };
            let p = scripts.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }

    let _ = candidates;
    None
}

/// Prepare a per-session ZDOTDIR containing VS Code's zsh integration as `.zshrc`.
fn setup_zsh_zdotdir(session_dir: &Path, si_rc: &Path) -> Result<PathBuf> {
    let zdotdir = session_dir.join("vscode-si-zdot");
    fs::create_dir_all(&zdotdir)
        .with_context(|| format!("create zsh ZDOTDIR {}", zdotdir.display()))?;
    let dest = zdotdir.join(".zshrc");
    // Prefer a hard link / copy so the session keeps working if VS Code updates.
    if dest.exists() {
        let _ = fs::remove_file(&dest);
    }
    match fs::copy(si_rc, &dest) {
        Ok(_) => {}
        Err(_) => {
            // Symlink as fallback (e.g. cross-device copy issues are unlikely here).
            #[cfg(unix)]
            {
                let _ = fs::remove_file(&dest);
                std::os::unix::fs::symlink(si_rc, &dest).with_context(|| {
                    format!("symlink {} -> {}", si_rc.display(), dest.display())
                })?;
            }
        }
    }

    // Optional siblings used for login shells / env; ignore errors.
    if let Some(parent) = si_rc.parent() {
        for name in [
            "shellIntegration-profile.zsh",
            "shellIntegration-env.zsh",
            "shellIntegration-login.zsh",
        ] {
            let src = parent.join(name);
            if src.is_file() {
                let dst_name = match name {
                    "shellIntegration-profile.zsh" => ".zprofile",
                    "shellIntegration-env.zsh" => ".zshenv",
                    "shellIntegration-login.zsh" => ".zlogin",
                    _ => continue,
                };
                let dst = zdotdir.join(dst_name);
                let _ = fs::copy(&src, &dst);
            }
        }
    }

    Ok(zdotdir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_finished_sequence_is_osc_633_d() {
        assert_eq!(OSC_633_COMMAND_FINISHED, b"\x1b]633;D\x07");
    }

    #[test]
    fn shell_kind_detects_bash_zsh() {
        assert_eq!(shell_kind("bash"), ShellKind::Bash);
        assert_eq!(shell_kind("zsh"), ShellKind::Zsh);
        assert_eq!(shell_kind("fish"), ShellKind::Other);
    }
}
