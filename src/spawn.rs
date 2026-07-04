use anyhow::{anyhow, Result};
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    Ghostty,
    ITerm2,
    TerminalApp,
    WezTerm,
    Kitty,
    Unknown,
}

pub fn detect() -> Terminal {
    if std::env::var("KITTY_WINDOW_ID").is_ok() {
        return Terminal::Kitty;
    }
    match std::env::var("TERM_PROGRAM").unwrap_or_default().as_str() {
        "ghostty" => Terminal::Ghostty,
        "iTerm.app" => Terminal::ITerm2,
        "Apple_Terminal" => Terminal::TerminalApp,
        "WezTerm" => Terminal::WezTerm,
        _ => Terminal::Unknown,
    }
}

/// Open a real terminal tab running `cmd`. Returns a human-readable status.
pub fn spawn_tab(cmd: &str) -> Result<String> {
    match detect() {
        Terminal::Ghostty => {
            spawn_ghostty(cmd)?;
            Ok("tab opened in Ghostty".to_string())
        }
        Terminal::ITerm2 => {
            spawn_iterm2(cmd)?;
            Ok("tab opened in iTerm2".to_string())
        }
        Terminal::TerminalApp => {
            spawn_terminal_app(cmd)?;
            Ok("window opened in Terminal.app".to_string())
        }
        Terminal::WezTerm => {
            spawn_argv("wezterm", &["cli", "spawn", "--"], cmd)?;
            Ok("tab opened in WezTerm".to_string())
        }
        Terminal::Kitty => {
            spawn_argv("kitty", &["@", "launch", "--type=tab"], cmd)?;
            Ok("tab opened in kitty (requires allow_remote_control)".to_string())
        }
        Terminal::Unknown => {
            pbcopy(cmd)?;
            Ok("no supported terminal detected — command copied to clipboard".to_string())
        }
    }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn osascript(script: &str) -> Result<()> {
    let out = Command::new("osascript").arg("-e").arg(script).output()?;
    if !out.status.success() {
        return Err(anyhow!(
            "osascript failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Ghostty >= 1.3 ships a native AppleScript dictionary; surface
/// configurations carry the command to run. Fallback for older versions:
/// a fresh instance window via `open -na Ghostty --args -e`.
fn spawn_ghostty(cmd: &str) -> Result<()> {
    let script = format!(
        r#"tell application "Ghostty"
    activate
    set cfg to new surface configuration
    set command of cfg to "{c}"
    set wait after command of cfg to true
    if (count of windows) > 0 then
        new tab in front window with configuration cfg
    else
        new window with configuration cfg
    end if
end tell"#,
        c = esc(cmd)
    );
    if osascript(&script).is_ok() {
        return Ok(());
    }
    let mut args: Vec<String> = vec![
        "-na".to_string(),
        "Ghostty".to_string(),
        "--args".to_string(),
        "-e".to_string(),
    ];
    args.extend(cmd.split_whitespace().map(str::to_string));
    let out = Command::new("open").args(&args).output()?;
    if !out.status.success() {
        return Err(anyhow!(
            "open -na Ghostty failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn spawn_iterm2(cmd: &str) -> Result<()> {
    let script = format!(
        r#"tell application "iTerm2"
    activate
    if (count of windows) = 0 then
        set win to (create window with default profile)
        tell current session of win to write text "{c}"
    else
        tell current window
            create tab with default profile
            tell current session to write text "{c}"
        end tell
    end if
end tell"#,
        c = esc(cmd)
    );
    osascript(&script)
}

fn spawn_terminal_app(cmd: &str) -> Result<()> {
    let script = format!(
        r#"tell application "Terminal"
    activate
    do script "{c}"
end tell"#,
        c = esc(cmd)
    );
    osascript(&script)
}

fn spawn_argv(bin: &str, prefix: &[&str], cmd: &str) -> Result<()> {
    let mut args: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
    args.extend(cmd.split_whitespace().map(str::to_string));
    let out = Command::new(bin).args(&args).output()?;
    if !out.status.success() {
        return Err(anyhow!(
            "{bin} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn pbcopy(text: &str) -> Result<()> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }
    child.wait()?;
    Ok(())
}
