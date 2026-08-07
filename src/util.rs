use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::process::Command;

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// Name the call that failed. Win32 errors are just codes -- "Access is denied"
/// with no idea which of six VirtDisk calls said it is not a diagnosis.
pub trait Ctx<T> {
    fn ctx(self, what: &str) -> Res<T>;
}

impl<T, E: std::fmt::Display> Ctx<T> for Result<T, E> {
    fn ctx(self, what: &str) -> Res<T> {
        self.map_err(|e| format!("{what}: {e}").into())
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Run a PowerShell snippet, return trimmed stdout.
///
/// ponytail: PowerShell is the shortest path to VSS and the Storage cmdlets,
/// and it exists on every live Windows. Ceiling: it is absent from a minimal
/// WinPE and costs ~400ms a call. Replace with direct IVssBackupComponents +
/// our own GPT writer once the partition-manager code lands (it needs a GPT
/// writer anyway).
pub fn ps(script: &str) -> Res<String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()?;
    if !out.status.success() {
        return Err(format!("powershell failed: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn human(b: u64) -> String {
    const U: [(&str, u64); 4] = [("TB", 1 << 40), ("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    for (n, d) in U {
        if b >= d { return format!("{:.1} {}", b as f64 / d as f64, n); }
    }
    format!("{b} B")
}
