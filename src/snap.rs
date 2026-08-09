//! Volume Shadow Copy: a frozen, consistent, read-only view of a live volume.
//! This is what lets you image C: while Windows is running on it.
use crate::util::{ps, Res};

pub struct Snapshot {
    pub id: String,
    /// e.g. `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy12`
    pub device: String,
}

/// What Win32_ShadowCopy.Create returned, in words.
///
/// Worth translating: the common answer on a USB stick is 4, and "specified
/// volume not supported" means VSS only does NTFS -- not that anything is
/// wrong. Callers that can fall back to a raw copy should say so and get on
/// with it rather than printing a page of PowerShell.
fn vss_error(code: &str) -> String {
    let why = match code {
        "1" => "access denied",
        "2" => "invalid argument",
        "3" => "volume not found",
        "4" => "volume not supported -- VSS snapshots NTFS only, not FAT/exFAT or removable media",
        "5" => "unsupported shadow copy context",
        "6" => "insufficient storage for the shadow copy",
        "7" => "volume is in use",
        "8" => "maximum number of shadow copies reached",
        "9" => "another shadow copy operation is already in progress",
        "10" => "a shadow copy provider vetoed it",
        "11" => "shadow copy provider not registered",
        "12" => "shadow copy provider failure",
        _ => "unknown error",
    };
    format!("VSS refused ({code}): {why}")
}

impl Snapshot {
    pub fn create(volume: &str) -> Res<Snapshot> {
        let vol = if volume.ends_with('\\') { volume.to_string() } else { format!("{volume}\\") };
        // Report the code rather than failing the script, so the caller gets
        // one line instead of PowerShell's rendering of the whole snippet.
        let out = ps(&format!(
            r#"$r = (Get-WmiObject -List Win32_ShadowCopy).Create('{vol}','ClientAccessible')
               if ($r.ReturnValue -ne 0) {{ "ERR $($r.ReturnValue)"; exit 0 }}
               $s = Get-CimInstance Win32_ShadowCopy | Where-Object {{ $_.ID -eq $r.ShadowID }}
               "$($s.ID)`n$($s.DeviceObject)""#
        ))?;
        if let Some(code) = out.trim().strip_prefix("ERR ") {
            return Err(vss_error(code.trim()).into());
        }
        let mut lines = out.lines();
        let id = lines.next().unwrap_or_default().trim().to_string();
        let device = lines.next().unwrap_or_default().trim().to_string();
        if device.is_empty() {
            return Err("VSS created a snapshot but reported no device path".into());
        }
        Ok(Snapshot { id, device })
    }

    pub fn delete(&self) -> Res<()> {
        ps(&format!(
            "Get-CimInstance Win32_ShadowCopy | Where-Object {{ $_.ID -eq '{}' }} | Remove-CimInstance",
            self.id
        ))?;
        Ok(())
    }
}
