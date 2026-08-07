//! bulkhead -- block-level backup and recovery for Windows.
//!
//! Images a live volume through a VSS snapshot into a VHDX. VHDX is the point:
//! Windows already mounts one as a drive, already does differencing chains for
//! incrementals, and already boots one. The paid tools charge for those.
mod snap;
mod util;
mod vhdx;

use std::ffi::c_void;
use std::io::Write as _;
use std::mem::size_of;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, SetFilePointerEx, WriteFile, FILE_BEGIN, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO};
use windows::Win32::System::IO::DeviceIoControl;

use snap::Snapshot;
use util::{human, ps, wide, Res};
use vhdx::Vhd;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const MB: u64 = 1 << 20;
const CHUNK: usize = 4 << 20;

/// A raw block device or volume handle. All I/O is sector-aligned by
/// construction: we start at an aligned offset and move in 4 MiB steps.
struct Raw(HANDLE);

impl Drop for Raw {
    fn drop(&mut self) {
        unsafe { let _ = CloseHandle(self.0); }
    }
}

impl Raw {
    fn open(path: &str, write: bool) -> Res<Raw> {
        let w = wide(path);
        let access = if write { GENERIC_READ | GENERIC_WRITE } else { GENERIC_READ };
        let h = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()), access, FILE_SHARE_READ | FILE_SHARE_WRITE, None,
                OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None,
            )?
        };
        Ok(Raw(h))
    }

    fn len(&self) -> Res<u64> {
        let mut li = GET_LENGTH_INFORMATION::default();
        let mut ret = 0u32;
        unsafe {
            DeviceIoControl(
                self.0, IOCTL_DISK_GET_LENGTH_INFO, None, 0,
                Some(&mut li as *mut _ as *mut c_void),
                size_of::<GET_LENGTH_INFORMATION>() as u32, Some(&mut ret), None,
            )?;
        }
        Ok(li.Length as u64)
    }

    fn seek(&self, off: u64) -> Res<()> {
        unsafe { SetFilePointerEx(self.0, off as i64, None, FILE_BEGIN)?; }
        Ok(())
    }

    fn read(&self, buf: &mut [u8]) -> Res<usize> {
        let mut n = 0u32;
        unsafe { ReadFile(self.0, Some(buf), Some(&mut n), None)?; }
        Ok(n as usize)
    }

    fn write_all(&self, buf: &[u8]) -> Res<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let mut n = 0u32;
            unsafe { WriteFile(self.0, Some(&buf[done..]), Some(&mut n), None)?; }
            if n == 0 { return Err("short write to target".into()); }
            done += n as usize;
        }
        Ok(())
    }
}

/// Copy `total` bytes from the start of `src` to `dst` at `dst_off`.
///
/// `skip_same` is what makes an incremental incremental: a differencing VHDX
/// serves the parent's content for any block it has not been written to, so
/// comparing before writing leaves unchanged blocks unallocated in the child.
/// ponytail: read-compare costs a full read of the parent. The upgrade is
/// changed-block tracking (a filter driver), which is a lot of driver for a
/// feature that is I/O-bound either way.
fn copy(src: &Raw, dst: &Raw, dst_off: u64, total: u64, skip_same: bool) -> Res<()> {
    let mut buf = vec![0u8; CHUNK];
    let mut old = if skip_same { vec![0u8; CHUNK] } else { Vec::new() };
    let (mut done, mut written) = (0u64, 0u64);
    let mut last_pct = u64::MAX;
    while done < total {
        let want = ((total - done) as usize).min(CHUNK);
        let n = src.read(&mut buf[..want])?;
        if n == 0 { return Err(format!("source ended early at {done} of {total}").into()); }

        dst.seek(dst_off + done)?;
        let same = skip_same && dst.read(&mut old[..n])? == n && old[..n] == buf[..n];
        if !same {
            dst.seek(dst_off + done)?;
            dst.write_all(&buf[..n])?;
            written += n as u64;
        }

        done += n as u64;
        let pct = done * 100 / total;
        if pct != last_pct {
            eprint!("\r  {pct:3}%  {} / {}", human(done), human(total));
            let _ = std::io::stderr().flush();
            last_pct = pct;
        }
    }
    eprintln!();
    if skip_same { eprintln!("[*] {} changed", human(written)); }
    Ok(())
}

fn cmd_image(volume: &str, out: &str, use_vss: bool, parent: Option<&str>) -> Res<()> {
    let shadow = if use_vss {
        eprintln!("[*] snapshotting {volume}");
        Some(Snapshot::create(volume)?)
    } else {
        None
    };
    // Bare `\\.\C:` (no trailing slash) is the raw volume; the shadow device is
    // already a raw path.
    let src_path = match &shadow {
        Some(s) => s.device.clone(),
        None => format!(r"\\.\{}", volume.trim_end_matches('\\').trim_end_matches(':')) + ":",
    };
    let r = image_inner(&src_path, out, parent);
    if let Some(s) = &shadow {
        eprintln!("[*] releasing snapshot");
        if let Err(e) = s.delete() { eprintln!("[!] snapshot {} not released: {e}", s.id); }
    }
    r
}

fn image_inner(src_path: &str, out: &str, parent: Option<&str>) -> Res<()> {
    let src = Raw::open(src_path, false)?;
    let vol_size = src.len()?;
    eprintln!("[*] source {src_path} ({})", human(vol_size));

    // Slack for the protective MBR + primary GPT (1 MiB alignment) and the
    // backup GPT at the tail.
    let disk_size = (vol_size + 8 * MB + MB - 1) / MB * MB;
    let vhd = match parent {
        Some(p) => { eprintln!("[*] incremental against {p}"); Vhd::create_diff(out, p)? }
        None => Vhd::create(out, disk_size)?,
    };
    vhd.attach(false, false, false)?;
    let disk = vhd.disk_number()?;

    // A differencing disk inherits the parent's partition table -- repartitioning
    // it would orphan the parent's data. Only a full image lays down a new one.
    // ponytail: Windows partitions its own disks correctly; see util::ps.
    let offset: u64 = if parent.is_some() {
        ps(&format!("(Get-Partition -DiskNumber {disk} | Sort-Object Offset | Select-Object -First 1).Offset"))?
    } else {
        ps(&format!(
            r#"Initialize-Disk -Number {disk} -PartitionStyle GPT -Confirm:$false | Out-Null
               (New-Partition -DiskNumber {disk} -UseMaximumSize -GptType '{{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}}').Offset"#
        ))?
    }.parse()?;
    let part_size = ps(&format!(
        "(Get-Partition -DiskNumber {disk} | Where-Object Offset -eq {offset}).Size"
    ))?.parse::<u64>()?;
    if part_size < vol_size {
        return Err(format!("partition {} < volume {}", human(part_size), human(vol_size)).into());
    }

    let dst = Raw::open(&vhd.physical_path()?, true)?;
    copy(&src, &dst, offset, vol_size, parent.is_some())?;
    drop(dst);

    vhd.detach()?;
    eprintln!("[+] {out}\n    mount it:  bulkhead mount {out}");
    Ok(())
}

fn cmd_mount(path: &str, rw: bool) -> Res<()> {
    let vhd = Vhd::open(path)?;
    vhd.attach(!rw, true, true)?;
    eprintln!("[+] attached {} ({})", vhd.physical_path()?, if rw { "read-write" } else { "read-only" });
    eprintln!("    it appears in Explorer; detach with:  bulkhead unmount {path}");
    Ok(())
}

fn cmd_unmount(path: &str) -> Res<()> {
    let vhd = Vhd::open(path)?;
    vhd.detach()?;
    eprintln!("[+] detached {path}");
    Ok(())
}

const USAGE: &str = "\
bulkhead -- block-level backup and recovery for Windows

  bulkhead image <VOL> <OUT.vhdx> [--from <PARENT.vhdx>] [--no-snapshot]
      Image a volume (e.g. C:) into a VHDX through a VSS snapshot.
      --from makes it an incremental: a VHDX differencing disk off PARENT.
      --no-snapshot images the volume directly (offline disks only).

  bulkhead mount <IMAGE.vhdx> [--rw]
      Attach the image as a drive. Read-only unless --rw.

  bulkhead unmount <IMAGE.vhdx>

Needs an elevated prompt (raw volume access).";

/// Positional args, with `--flags` and `--from VALUE` removed.
fn positional<'a>(a: &[&'a str]) -> Vec<&'a str> {
    let mut v = Vec::new();
    let mut it = a.iter().copied();
    while let Some(x) = it.next() {
        match x {
            "--from" => { it.next(); }
            _ if x.starts_with("--") => {}
            _ => v.push(x),
        }
    }
    v
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let flag = |f: &str| a.contains(&f);
    let opt = |f: &str| a.iter().position(|&x| x == f).and_then(|i| a.get(i + 1)).copied();
    let pos = positional(&a);

    let r = match pos.as_slice() {
        ["image", vol, out] => cmd_image(vol, out, !flag("--no-snapshot"), opt("--from")),
        ["mount", img] => cmd_mount(img, flag("--rw")),
        ["unmount", img] => cmd_unmount(img),
        _ => { eprintln!("{USAGE}"); std::process::exit(2); }
    };
    if let Err(e) = r {
        eprintln!("[!] {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args() {
        assert_eq!(positional(&["image", "C:", "o.vhdx"]), ["image", "C:", "o.vhdx"]);
        // --from swallows its value; bare flags vanish
        assert_eq!(
            positional(&["image", "C:", "o.vhdx", "--from", "p.vhdx", "--no-snapshot"]),
            ["image", "C:", "o.vhdx"]
        );
        // a flag wedged between positionals must not eat one
        assert_eq!(positional(&["mount", "--rw", "o.vhdx"]), ["mount", "o.vhdx"]);
        // trailing --from with no value must not panic
        assert_eq!(positional(&["mount", "o.vhdx", "--from"]), ["mount", "o.vhdx"]);
    }

    #[test]
    fn sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1 << 30), "1.0 GB");
        // VHDX must be >= volume + GPT slack, and 1 MiB aligned
        let d = |v: u64| (v + 8 * MB + MB - 1) / MB * MB;
        assert!(d(100 * MB + 1) > 100 * MB && d(100 * MB + 1) % MB == 0);
    }
}
