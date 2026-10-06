//! Login-shell environment for launches from a desktop menu, which inherit only the session
//! manager's environment (no `~/.profile` PATH entries, no `DEEPSEEK_API_KEY` exported in rc files).
//! Mirrors `apps/desktop/src/login-shell-environment.ts`.

use std::collections::HashMap;
use std::ffi::CStr;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DELIMITER: &str = "_DSH_SHELL_ENV_DELIMITER_";
const FALLBACK_SHELLS: [&str; 3] = ["/bin/zsh", "/bin/bash", "/bin/sh"];
/// Keep oh-my-zsh update prompts and tmux autostart plugins from blocking a non-terminal read.
const PROBE_ENVIRONMENT: [(&str, &str); 3] = [
    ("DISABLE_AUTO_UPDATE", "true"),
    ("ZSH_TMUX_AUTOSTARTED", "true"),
    ("ZSH_TMUX_AUTOSTART", "false"),
];
/// Variables describing the probe shell process rather than the user's configuration.
const SHELL_SESSION_KEYS: [&str; 4] = ["PWD", "OLDPWD", "SHLVL", "_"];
/// The launcher resolves these itself; the shell must not override them.
const LAUNCHER_OWNED_PREFIXES: [&str; 2] = ["DSH_", "TAURI_"];

/// Inherited environment overlaid with the first login shell that produces one.
/// Returns the inherited environment and the per-shell failure reasons.
pub fn login_environment(timeout: Duration) -> (HashMap<String, String>, Vec<String>) {
    let mut merged: HashMap<String, String> = std::env::vars().collect();
    let mut failures = Vec::new();
    for shell in candidates() {
        match read_shell(&shell, timeout) {
            Ok(vars) => {
                for (key, value) in vars {
                    let probe = PROBE_ENVIRONMENT.iter().any(|(k, _)| *k == key);
                    if probe
                        || SHELL_SESSION_KEYS.contains(&key.as_str())
                        || LAUNCHER_OWNED_PREFIXES.iter().any(|p| key.starts_with(p))
                    {
                        continue;
                    }
                    merged.insert(key, value);
                }
                return (merged, failures);
            }
            Err(reason) => failures.push(format!("{shell}: {reason}")),
        }
    }
    (merged, failures)
}

/// The account's login shell from the passwd database, then fixed system shells.
fn candidates() -> Vec<String> {
    let mut shells = Vec::new();
    // SAFETY: getpwuid returns a pointer into static storage or null; it is read immediately.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if !entry.is_null() && !(*entry).pw_shell.is_null() {
            if let Ok(shell) = CStr::from_ptr((*entry).pw_shell).to_str() {
                if !shell.is_empty() {
                    shells.push(shell.to_owned());
                }
            }
        }
    }
    for shell in FALLBACK_SHELLS {
        if !shells.iter().any(|s| s == shell) && std::path::Path::new(shell).exists() {
            shells.push(shell.to_owned());
        }
    }
    shells
}

fn read_shell(shell: &str, timeout: Duration) -> Result<HashMap<String, String>, String> {
    let dump = format!(
        "printf '\\0%s\\0' '{DELIMITER}'; command env -0 || exit; printf '\\0%s\\0' '{DELIMITER}'; exit"
    );
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
    let mut child = Command::new(shell)
        .args(["-ilc", &dump])
        .envs(PROBE_ENVIRONMENT)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Own process group: a timeout kills rc-file background jobs along with the shell.
        .process_group(0)
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // SAFETY: negative pid targets the process group created above.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                return Err("timeout".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error.to_string()),
        }
    };
    let output = reader.join().map_err(|_| "reader panicked".to_string())?;
    parse(&output).ok_or_else(|| match status.code() {
        Some(code) if code != 0 => format!("exit {code}"),
        _ => "unparsed".into(),
    })
}

/// Variables between the two delimiters; rc-file noise around the dump is ignored.
fn parse(output: &[u8]) -> Option<HashMap<String, String>> {
    let parts: Vec<&[u8]> = output.split(|b| *b == 0).collect();
    let first = parts.iter().position(|p| *p == DELIMITER.as_bytes())?;
    let last = parts.iter().rposition(|p| *p == DELIMITER.as_bytes())?;
    if first == last {
        return None;
    }
    let mut vars = HashMap::new();
    for entry in &parts[first + 1..last] {
        let Ok(entry) = std::str::from_utf8(entry) else { continue };
        if let Some((key, value)) = entry.split_once('=') {
            if !key.is_empty() {
                vars.insert(key.to_owned(), value.to_owned());
            }
        }
    }
    Some(vars)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_only_between_delimiters() {
        let out = b"motd noise\0_DSH_SHELL_ENV_DELIMITER_\0PATH=/a:/b\0EQ=x=y\0junk\0_DSH_SHELL_ENV_DELIMITER_\0bye";
        let vars = parse(out).expect("parsed");
        assert_eq!(vars["PATH"], "/a:/b");
        assert_eq!(vars["EQ"], "x=y");
        assert_eq!(vars.len(), 2);
    }

    #[test]
    fn rejects_truncated_dump() {
        assert!(parse(b"\0_DSH_SHELL_ENV_DELIMITER_\0PATH=/a").is_none());
    }
}
