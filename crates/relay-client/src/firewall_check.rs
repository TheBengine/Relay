//! Windows Firewall and the account-free connection.
//!
//! When Windows first sees `relay-iroh.exe`, it asks whether to allow it, and an
//! answer for "private networks only" leaves a block rule for public ones. If
//! this PC's network is marked Public, friends can still join, but direct
//! connections may fail and calls then go through a relay server. Relay checks
//! for such a block rule and, only when the host asks, replaces it with an allow
//! rule for this one program. Windows asks for permission (UAC) first.
use std::path::{Path, PathBuf};

/// What the check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Firewall {
    /// Nothing blocks the account-free connection on the networks in use.
    Open,
    /// An enabled inbound block rule for the helper applies to a network in use.
    Blocked,
}

/// The helper whose rules are checked, next to Relay itself.
pub fn helper() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("relay-iroh.exe"))
}

/// Single quotes for a PowerShell literal: the only character to double is `'`.
fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

/// Reads rules only. Prints BLOCKED or OPEN.
pub fn check_script(helper: &Path) -> String {
    format!(
        r#"$ErrorActionPreference = 'SilentlyContinue'
$exe = {exe}
$categories = @(Get-NetConnectionProfile | ForEach-Object {{ ([string]$_.NetworkCategory) -replace 'DomainAuthenticated','Domain' }})
$blocked = $false
foreach ($rule in @(Get-NetFirewallApplicationFilter -Program $exe | Get-NetFirewallRule)) {{
  if ([string]$rule.Enabled -ne 'True' -or [string]$rule.Direction -ne 'Inbound' -or [string]$rule.Action -ne 'Block') {{ continue }}
  $profiles = [string]$rule.Profile
  if ($profiles -eq 'Any') {{ $blocked = $true }}
  foreach ($category in $categories) {{ if ($profiles -match $category) {{ $blocked = $true }} }}
}}
if ($blocked) {{ 'BLOCKED' }} else {{ 'OPEN' }}
"#,
        exe = quoted(helper)
    )
}

/// Runs elevated: removes inbound block rules for this program only and allows
/// it inbound. Nothing else is touched.
pub fn allow_script(helper: &Path) -> String {
    format!(
        r#"$ErrorActionPreference = 'Stop'
$exe = {exe}
Get-NetFirewallApplicationFilter -Program $exe -ErrorAction SilentlyContinue | Get-NetFirewallRule -ErrorAction SilentlyContinue | Where-Object {{ [string]$_.Direction -eq 'Inbound' -and [string]$_.Action -eq 'Block' }} | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName 'Relay without accounts (iroh)' -Direction Inbound -Program $exe -Action Allow -Profile Any | Out-Null
"#,
        exe = quoted(helper)
    )
}

pub fn parse(output: &str) -> Option<Firewall> {
    match output
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())?
    {
        "BLOCKED" => Some(Firewall::Blocked),
        "OPEN" => Some(Firewall::Open),
        _ => None,
    }
}

/// PowerShell's -EncodedCommand: UTF-16LE in base64, so no quoting can break.
fn encoded(script: &str) -> String {
    use base64::Engine as _;
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(windows)]
fn powershell(arguments: &[&str]) -> Result<std::process::Output, String> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive"])
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .creation_flags(0x08000000)
        .output()
        .map_err(|_| "Relay could not check Windows Firewall.".to_string())
}

/// Reads the rules (no change, no prompt).
#[cfg(windows)]
pub fn check() -> Result<Firewall, String> {
    let helper = helper().filter(|p| p.is_file()).ok_or("not included")?;
    let output = powershell(&["-EncodedCommand", &encoded(&check_script(&helper))])?;
    parse(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| "Relay could not read Windows Firewall.".to_string())
}

/// Asks Windows for permission and allows the helper. Waits for the answer.
#[cfg(windows)]
pub fn allow() -> Result<(), String> {
    let helper = helper().filter(|p| p.is_file()).ok_or("not included")?;
    let inner = encoded(&allow_script(&helper));
    let outer = format!(
        "$p = Start-Process powershell.exe -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile','-NonInteractive','-EncodedCommand','{inner}'; exit $p.ExitCode"
    );
    let output = powershell(&["-EncodedCommand", &encoded(&outer)])?;
    if output.status.success() {
        Ok(())
    } else {
        Err("Windows did not allow the change. Nothing was changed.".into())
    }
}

#[cfg(not(windows))]
pub fn check() -> Result<Firewall, String> {
    Ok(Firewall::Open)
}

#[cfg(not(windows))]
pub fn allow() -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_name_only_the_helper_and_survive_quotes_in_the_path() {
        let path = Path::new(r"C:\Users\O'Brien\Relay\dist\relay-iroh.exe");
        for script in [check_script(path), allow_script(path)] {
            assert!(
                script.contains(r"$exe = 'C:\Users\O''Brien\Relay\dist\relay-iroh.exe'"),
                "{script}"
            );
            // Rules are only ever looked up through this one program.
            assert_eq!(
                script.matches("-Program $exe").count(),
                if script.contains("New-NetFirewallRule") {
                    2
                } else {
                    1
                }
            );
        }
        let check = check_script(path);
        assert!(
            !check.contains("Remove-NetFirewallRule") && !check.contains("New-NetFirewallRule")
        );
        let allow = allow_script(path);
        assert!(allow.contains("Action -eq 'Block'") && allow.contains("-Action Allow"));
    }

    #[test]
    fn only_a_clear_answer_counts() {
        assert_eq!(parse("BLOCKED\r\n"), Some(Firewall::Blocked));
        assert_eq!(parse("noise\nOPEN\n"), Some(Firewall::Open));
        assert_eq!(parse(""), None);
        assert_eq!(parse("Access denied"), None);
    }

    #[test]
    fn encoded_commands_are_utf16_base64() {
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded("'OPEN'"))
            .unwrap();
        assert_eq!(raw, b"'\0O\0P\0E\0N\0'\0");
    }

    /// Reads this PC's real firewall rules; changes nothing.
    #[test]
    #[cfg(windows)]
    #[ignore = "reads this PC's firewall; run with --ignored"]
    fn the_check_runs_against_windows() {
        let script = check_script(Path::new(r"C:\Windows\System32\notepad.exe"));
        let output = powershell(&["-EncodedCommand", &encoded(&script)]).unwrap();
        assert!(parse(&String::from_utf8_lossy(&output.stdout)).is_some());
    }
}
