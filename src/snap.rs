//! Volume Shadow Copy: a frozen, consistent, read-only view of a live volume.
//! This is what lets you image C: while Windows is running on it.
use crate::util::{ps, Res};

pub struct Snapshot {
    pub id: String,
    /// e.g. `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy12`
    pub device: String,
}

impl Snapshot {
    pub fn create(volume: &str) -> Res<Snapshot> {
        let vol = if volume.ends_with('\\') { volume.to_string() } else { format!("{volume}\\") };
        let out = ps(&format!(
            r#"$r = (Get-WmiObject -List Win32_ShadowCopy).Create('{vol}','ClientAccessible')
               if ($r.ReturnValue -ne 0) {{ Write-Error "VSS Create returned $($r.ReturnValue)"; exit 1 }}
               $s = Get-CimInstance Win32_ShadowCopy | Where-Object {{ $_.ID -eq $r.ShadowID }}
               "$($s.ID)`n$($s.DeviceObject)""#
        ))?;
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
