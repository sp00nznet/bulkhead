//! Thin wrapper over the Windows VirtDisk API. Windows already implements
//! VHDX: dynamic allocation, differencing chains, and mount-as-a-drive. We
//! just call it.
use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::Vhd::*;

use crate::util::{wide, Ctx, Res};

const VENDOR_MSFT: GUID = GUID::from_u128(0xec984aec_a0f9_47e9_901f_71415a66345b);

fn vhdx_type() -> VIRTUAL_STORAGE_TYPE {
    VIRTUAL_STORAGE_TYPE { DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX, VendorId: VENDOR_MSFT }
}

/// An open VirtDisk handle. Detach-on-drop is deliberately NOT done: `image`
/// detaches explicitly when the copy succeeds, `mount` attaches with
/// PERMANENT_LIFETIME so the drive outlives us.
pub struct Vhd(pub HANDLE);

impl Drop for Vhd {
    fn drop(&mut self) {
        unsafe { let _ = CloseHandle(self.0); }
    }
}

impl Vhd {
    /// Create a new dynamically-expanding VHDX. `size` is the virtual size.
    /// The handle is closed straight away -- attaching goes through `open`.
    pub fn create(path: &str, size: u64) -> Res<()> {
        let w = wide(path);
        let mut p = CREATE_VIRTUAL_DISK_PARAMETERS::default();
        p.Version = CREATE_VIRTUAL_DISK_VERSION_2;
        p.Anonymous.Version2.MaximumSize = size;
        p.Anonymous.Version2.BlockSizeInBytes = 0; // provider default (32 MiB)
        p.Anonymous.Version2.SectorSizeInBytes = 512;
        p.Anonymous.Version2.PhysicalSectorSizeInBytes = 512;
        let mut h = HANDLE::default();
        unsafe {
            CreateVirtualDisk(
                &vhdx_type(), PCWSTR(w.as_ptr()), VIRTUAL_DISK_ACCESS_NONE, None,
                CREATE_VIRTUAL_DISK_FLAG_NONE, 0, &p, None, &mut h,
            ).ok().ctx("CreateVirtualDisk")?;
        }
        drop(Vhd(h));
        Ok(())
    }

    /// Create a differencing VHDX whose backing store is `parent`. This is how
    /// incremental chains work -- Windows does the block tracking.
    pub fn create_diff(path: &str, parent: &str) -> Res<()> {
        let w = wide(path);
        let pw = wide(parent);
        let mut p = CREATE_VIRTUAL_DISK_PARAMETERS::default();
        p.Version = CREATE_VIRTUAL_DISK_VERSION_2;
        p.Anonymous.Version2.ParentPath = PCWSTR(pw.as_ptr());
        p.Anonymous.Version2.ParentVirtualStorageType = vhdx_type();
        let mut h = HANDLE::default();
        unsafe {
            CreateVirtualDisk(
                &vhdx_type(), PCWSTR(w.as_ptr()), VIRTUAL_DISK_ACCESS_NONE, None,
                CREATE_VIRTUAL_DISK_FLAG_NONE, 0, &p, None, &mut h,
            ).ok().ctx("CreateVirtualDisk (differencing)")?;
        }
        drop(Vhd(h));
        Ok(())
    }

    /// Open for attaching.
    ///
    /// V1 parameters on purpose. A create-handle carries VIRTUAL_DISK_ACCESS_NONE,
    /// and attaching a differencing disk makes Windows open the whole parent
    /// chain using that mask -- which is how you get ERROR_ACCESS_DENIED from
    /// AttachVirtualDisk on a child that created just fine. V1 is what takes an
    /// explicit mask, and `RWDepth` = 1 is exactly the differencing case: this
    /// disk writable, its parents read-only.
    pub fn open(path: &str, writable: bool) -> Res<Vhd> {
        let w = wide(path);
        let access = if writable {
            VIRTUAL_DISK_ACCESS_ALL
        } else {
            VIRTUAL_DISK_ACCESS_ATTACH_RO | VIRTUAL_DISK_ACCESS_READ | VIRTUAL_DISK_ACCESS_DETACH
        };
        let mut p = OPEN_VIRTUAL_DISK_PARAMETERS::default();
        p.Version = OPEN_VIRTUAL_DISK_VERSION_1;
        p.Anonymous.Version1.RWDepth = if writable { 1 } else { 0 };
        let mut h = HANDLE::default();
        unsafe {
            OpenVirtualDisk(
                &vhdx_type(), PCWSTR(w.as_ptr()), access,
                OPEN_VIRTUAL_DISK_FLAG_NONE, Some(&p), &mut h,
            ).ok().ctx("OpenVirtualDisk")?;
        }
        Ok(Vhd(h))
    }

    /// `letter` = false attaches with no drive letter (raw block access, used
    /// while imaging); `permanent` keeps the disk attached after we exit.
    pub fn attach(&self, read_only: bool, letter: bool, permanent: bool) -> Res<()> {
        let mut flags = ATTACH_VIRTUAL_DISK_FLAG_NONE;
        if read_only { flags |= ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY; }
        if !letter { flags |= ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER; }
        if permanent { flags |= ATTACH_VIRTUAL_DISK_FLAG_PERMANENT_LIFETIME; }
        let mut p = ATTACH_VIRTUAL_DISK_PARAMETERS::default();
        p.Version = ATTACH_VIRTUAL_DISK_VERSION_1;
        unsafe { AttachVirtualDisk(self.0, None, flags, 0, Some(&p), None).ok().ctx("AttachVirtualDisk")?; }
        Ok(())
    }

    pub fn detach(&self) -> Res<()> {
        unsafe { DetachVirtualDisk(self.0, DETACH_VIRTUAL_DISK_FLAG_NONE, 0).ok().ctx("DetachVirtualDisk")?; }
        Ok(())
    }

    /// `\\.\PhysicalDriveN` for the attached disk.
    pub fn physical_path(&self) -> Res<String> {
        let mut buf = [0u16; 260];
        let mut len = (buf.len() * 2) as u32;
        unsafe { GetVirtualDiskPhysicalPath(self.0, &mut len, PWSTR(buf.as_mut_ptr())).ok().ctx("GetVirtualDiskPhysicalPath")?; }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(String::from_utf16_lossy(&buf[..n]))
    }

    /// Disk number parsed out of the physical path, for the Storage cmdlets.
    pub fn disk_number(&self) -> Res<u32> {
        let p = self.physical_path()?;
        p.rsplit(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|d| d.parse().ok())
            .ok_or_else(|| format!("no disk number in {p:?}").into())
    }
}
