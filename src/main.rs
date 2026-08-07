//! bulkhead -- block-level backup and recovery for Windows.
//!
//! Images a live volume through a VSS snapshot into a VHDX. VHDX is the point:
//! Windows already mounts one as a drive, already does differencing chains for
//! incrementals, and already boots one. The paid tools charge for those.
mod bitmap;
mod media;
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
use windows::Win32::System::Ioctl::{
    DISK_GEOMETRY, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY,
    IOCTL_DISK_GET_LENGTH_INFO,
};
use windows::Win32::System::IO::DeviceIoControl;

use bitmap::Bitmap;
use snap::Snapshot;
use util::{human, ps, wide, Ctx, Res};
use vhdx::Vhd;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const MB: u64 = 1 << 20;
const CHUNK: usize = 4 << 20;

/// Comparison granularity for incrementals. Small enough that a few changed
/// bytes do not drag a whole chunk along, large enough that the run list stays
/// short. ponytail: picked, not measured -- tune against a real workload.
const GRAIN: usize = 64 << 10;

/// GPT "Basic data partition". We tag the payload partition with it so we can
/// find it again on an incremental without tripping over the Microsoft
/// Reserved partition that Initialize-Disk creates alongside it.
const DATA_GUID: &str = "{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}";

/// How much of a volume's tail may be unreadable before we call it truncation
/// rather than a driver quirk. Observed gap is one cluster; 1 MiB is slack.
/// ponytail: guessing at the boundary. The precise answer is the filesystem's
/// own recorded length, which means parsing a BPB per filesystem.
const TAIL_SLACK: u64 = MB;

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
            ).ctx("IOCTL_DISK_GET_LENGTH_INFO")?;
        }
        Ok(li.Length as u64)
    }

    /// Logical bytes-per-sector. A whole-disk image must declare the source's
    /// value or every LBA in the copied GPT points somewhere else.
    fn sector_size(&self) -> Res<u32> {
        let mut g = DISK_GEOMETRY::default();
        let mut ret = 0u32;
        unsafe {
            DeviceIoControl(
                self.0, IOCTL_DISK_GET_DRIVE_GEOMETRY, None, 0,
                Some(&mut g as *mut _ as *mut c_void),
                size_of::<DISK_GEOMETRY>() as u32, Some(&mut ret), None,
            ).ctx("IOCTL_DISK_GET_DRIVE_GEOMETRY")?;
        }
        Ok(g.BytesPerSector)
    }

    fn seek(&self, off: u64) -> Res<()> {
        unsafe { SetFilePointerEx(self.0, off as i64, None, FILE_BEGIN).ctx("seek")?; }
        Ok(())
    }

    fn read(&self, buf: &mut [u8]) -> Res<usize> {
        let mut n = 0u32;
        unsafe { ReadFile(self.0, Some(buf), Some(&mut n), None).ctx("read")?; }
        Ok(n as usize)
    }

    fn write_all(&self, buf: &[u8]) -> Res<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let mut n = 0u32;
            unsafe { WriteFile(self.0, Some(&buf[done..]), Some(&mut n), None).ctx("write")?; }
            if n == 0 { return Err("short write to target".into()); }
            done += n as usize;
        }
        Ok(())
    }
}

/// Byte ranges of `new` that differ from `old`, compared in `grain` steps and
/// coalesced into runs. An empty `old` means everything differs, so a full copy
/// comes back as a single run and costs one write.
///
/// The granularity is the whole point: compare a 4 MiB chunk as one unit and a
/// single changed byte of NTFS metadata dirties all 4 MiB, which on a quiet
/// volume marks nearly the entire disk as changed.
fn diff_runs(old: &[u8], new: &[u8], grain: usize) -> Vec<(usize, usize)> {
    let n = new.len();
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    let mut i = 0usize;
    loop {
        let at_end = i >= n;
        let e = (i + grain).min(n);
        let differs = !at_end && (old.len() < e || old[i..e] != new[i..e]);
        match (differs, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => { runs.push((s, i)); start = None; }
            _ => {}
        }
        if at_end { break; }
        i = e;
    }
    runs
}

/// One region of a copy: `len` bytes from `src` at `src_off`, landing on `dst`
/// at `dst_off`. A volume image is a single region; a whole-disk image is one
/// per partition plus one per gap between them.
struct Region<'a> {
    src: &'a Raw,
    src_off: u64,
    dst: &'a Raw,
    dst_off: u64,
    len: u64,
    /// Compare against what is already there and write only what differs. This
    /// is what makes an incremental incremental: a differencing VHDX serves the
    /// parent's content for any block it has not been written to, so unchanged
    /// blocks stay unallocated in the child.
    /// ponytail: read-compare costs a full read of the parent. The upgrade is
    /// changed-block tracking (a filter driver), which is a lot of driver for
    /// something that is I/O-bound either way.
    delta: bool,
    /// Clusters the filesystem says are free are not read and not written.
    alloc: Option<&'a Bitmap>,
    /// How much of the tail may be unreadable before it counts as truncation.
    /// A volume serves slightly less than it reports; a raw disk does not, and
    /// its last sectors hold the backup GPT -- so this is 0 for raw regions.
    tail_slack: u64,
    label: &'a str,
}

impl Region<'_> {
    fn run(&self) -> Res<()> {
        let total = self.len;
        let mut buf = vec![0u8; CHUNK];
        let mut old = if self.delta { vec![0u8; CHUNK] } else { Vec::new() };
        let (mut done, mut written, mut skipped) = (0u64, 0u64, 0u64);
        let mut short = 0u64;
        let mut last_pct = u64::MAX;

        eprintln!("[*] {} ({})", self.label, human(total));
        while done < total {
            let pct = done * 100 / total;
            if pct != last_pct {
                eprint!("\r  {pct:3}%  {} / {}", human(done), human(total));
                let _ = std::io::stderr().flush();
                last_pct = pct;
            }
            let want = ((total - done) as usize).min(CHUNK);

            // Free space holds nothing worth copying. Skipping means not even
            // reading it, which is where the time goes on a mostly-empty disk.
            // ponytail: whole chunks only, so free space in runs shorter than
            // 4 MiB is still copied. Drop to GRAIN if that shows up.
            if self.alloc.is_some_and(|b| !b.any_allocated(done, done + want as u64)) {
                done += want as u64;
                skipped += want as u64;
                continue;
            }

            // Explicit, because a skipped chunk leaves the pointer behind.
            self.src.seek(self.src_off + done).ctx("source")?;
            let n = self.src.read(&mut buf[..want]).ctx("source")?;
            if n == 0 {
                // A volume reports its partition length but serves reads only
                // to the filesystem's own end, and refuses a straddling read
                // outright rather than returning a partial. Tolerate that at
                // the tail only; anywhere else a zero read is a truncated
                // image and must not pass silently.
                if total - done > self.tail_slack {
                    return Err(format!(
                        "{}: source ended early at {done} of {total}", self.label).into());
                }
                eprintln!("
[*] last {} not served by the volume driver; left zeroed",
                          human(total - done));
                break;
            }

            let compare = if self.delta {
                self.dst.seek(self.dst_off + done).ctx("target")?;
                let got = self.dst.read(&mut old[..n]).ctx("target readback")?;
                // A short readback is not wrong, just wasteful: the chunk gets
                // written whole instead of by run.
                if got != n {
                    short += 1;
                }
                got == n
            } else {
                false
            };

            let cmp: &[u8] = if compare { &old[..n] } else { &[] };
            for (s, e) in diff_runs(cmp, &buf[..n], GRAIN) {
                self.dst.seek(self.dst_off + done + s as u64).ctx("target")?;
                self.dst.write_all(&buf[s..e]).ctx("target")?;
                written += (e - s) as u64;
            }

            done += n as u64;
        }
        eprintln!("\r  100%  {} / {}      ", human(done), human(total));
        if let Some(b) = self.alloc {
            eprintln!("    {} free space skipped ({} of {} clusters in use)",
                      human(skipped), b.allocated, b.clusters);
        }
        if self.delta {
            eprintln!("    {} changed", human(written));
            if short > 0 {
                eprintln!("[!] {short} chunks could not be read back and were copied whole");
            }
        }
        Ok(())
    }
}


/// `disk0`, `0`, or `\\.\PhysicalDrive0` -- anything else is a volume.
fn disk_arg(s: &str) -> Option<u32> {
    let t = s.to_ascii_lowercase();
    let d = t.strip_prefix(r"\\.\physicaldrive")
        .or_else(|| t.strip_prefix("disk"))
        .unwrap_or(&t);
    if d.is_empty() { return None; }
    d.parse().ok()
}

struct Part {
    offset: u64,
    size: u64,
    /// Present only if Windows has the volume mounted, which is what makes it
    /// snapshottable. ESP, MSR and recovery partitions have none.
    letter: Option<char>,
}

fn partitions(disk: u32) -> Res<Vec<Part>> {
    let out = ps(&format!(
        "Get-Partition -DiskNumber {disk} | Sort-Object Offset |          ForEach-Object {{ \"$($_.Offset) $($_.Size) $($_.DriveLetter)\" }}"
    ))?;
    let mut v = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 2 { continue; }
        v.push(Part {
            offset: f[0].parse()?,
            size: f[1].parse()?,
            letter: f.get(2).and_then(|s| s.chars().next()).filter(|c| c.is_ascii_alphabetic()),
        });
    }
    Ok(v)
}

/// Image a whole disk: the partition table, every partition, and the gaps.
///
/// Mounted volumes are read through their own VSS snapshot so a live system
/// stays consistent; everything else is copied raw. The GPT is copied verbatim
/// rather than rebuilt, so the VHDX is the same size as the source and its
/// backup GPT lands on the same LBA -- which is what makes the image directly
/// bootable instead of merely restorable.
fn image_disk(disk: u32, out: &str, use_vss: bool, parent: Option<&str>) -> Res<()> {
    let phys_path = format!(r"\\.\PhysicalDrive{disk}");
    let phys = Raw::open(&phys_path, false).ctx("open source disk")?;
    let disk_size = phys.len()?;
    let sector = phys.sector_size()?;
    eprintln!("[*] source {phys_path} ({}, {sector}-byte sectors)", human(disk_size));

    let parts = partitions(disk)?;
    if parts.is_empty() {
        return Err(format!("disk {disk} has no partitions").into());
    }

    // Snapshot every mounted volume before copying anything, so the whole
    // image is one point in time rather than one per partition.
    let mut shadows: Vec<(usize, Snapshot)> = Vec::new();
    if use_vss {
        for (i, p) in parts.iter().enumerate() {
            let Some(l) = p.letter else { continue };
            eprintln!("[*] snapshotting {l}:");
            match Snapshot::create(&format!("{l}:")) {
                Ok(sn) => shadows.push((i, sn)),
                Err(e) => eprintln!("[!] {l}: no snapshot ({e}); copying it raw instead"),
            }
        }
    }

    let r = image_disk_inner(&phys, disk_size, sector, &parts, &shadows, out, parent);

    for (_, sn) in &shadows {
        if let Err(e) = sn.delete() {
            eprintln!("[!] snapshot {} not released: {e}", sn.id);
        }
    }
    r
}

fn image_disk_inner(phys: &Raw, disk_size: u64, sector: u32, parts: &[Part],
                    shadows: &[(usize, Snapshot)], out: &str, parent: Option<&str>) -> Res<()> {
    match parent {
        Some(p) => { eprintln!("[*] incremental against {p}"); Vhd::create_diff(out, p)?; }
        None => Vhd::create(out, disk_size, sector)?,
    }
    let vhd = Vhd::open(out, true)?;
    vhd.attach(false, false, false)?;
    let dnum = vhd.disk_number()?;
    // No Initialize-Disk here: the source's own partition table is copied
    // verbatim. Offline still matters, so nothing gets mounted mid-write.
    ps(&format!(
        "Set-Disk -Number {dnum} -IsOffline $true
         Set-Disk -Number {dnum} -IsReadOnly $false"
    ))?;
    let dst = Raw::open(&vhd.physical_path()?, true).ctx("open attached vhdx")?;
    let delta = parent.is_some();

    let raw = |off: u64, len: u64, label: &str| -> Res<()> {
        Region { src: phys, src_off: off, dst: &dst, dst_off: off, len,
                 delta, alloc: None, tail_slack: 0, label }.run()
    };

    let mut pos = 0u64;
    for (i, p) in parts.iter().enumerate() {
        if p.offset > pos {
            raw(pos, p.offset - pos, "gap")?;
        }
        match shadows.iter().find(|(j, _)| *j == i) {
            Some((_, sn)) => {
                let src = Raw::open(&sn.device, false).ctx("open snapshot")?;
                let vol_size = src.len()?.min(p.size);
                let alloc = bitmap::read(src.0, vol_size)?;
                let label = format!("partition {} ({}:)", i + 1, p.letter.unwrap_or('?'));
                Region { src: &src, src_off: 0, dst: &dst, dst_off: p.offset, len: vol_size,
                         delta, alloc: alloc.as_ref(), tail_slack: TAIL_SLACK,
                         label: &label }.run()?;
            }
            None => raw(p.offset, p.size, &format!("partition {} (raw)", i + 1))?,
        }
        pos = p.offset + p.size;
    }
    if pos < disk_size {
        // Includes the backup GPT on the last sectors, so it must not be
        // skipped or truncated.
        raw(pos, disk_size - pos, "tail + backup GPT")?;
    }

    drop(dst);
    vhd.detach()?;
    eprintln!("[+] {out}
    mount it:  bulkhead mount {out}");
    Ok(())
}

fn cmd_image(volume: &str, out: &str, use_vss: bool, parent: Option<&str>) -> Res<()> {
    if let Some(n) = disk_arg(volume) {
        return image_disk(n, out, use_vss, parent);
    }
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
    let src = Raw::open(src_path, false).ctx("open source volume")?;
    let vol_size = src.len()?;
    eprintln!("[*] source {src_path} ({})", human(vol_size));

    // Ask the filesystem which clusters actually hold data. None means it did
    // not offer a bitmap, and we image every sector instead.
    let alloc = bitmap::read(src.0, vol_size)?;
    match &alloc {
        Some(b) => eprintln!("[*] {} in use of {} ({}-byte clusters)",
                             human(b.allocated * b.cluster), human(vol_size), b.cluster),
        None => eprintln!("[*] no allocation bitmap; imaging every sector"),
    }

    // Slack for 1 MiB alignment, the backup GPT at the tail, and the Microsoft
    // Reserved partition Initialize-Disk inserts (16 MiB under 16 GB, 32 MiB
    // over). Over-reserving is free: the VHDX is dynamic.
    let disk_size = (vol_size + 40 * MB + MB - 1) / MB * MB;
    match parent {
        Some(p) => { eprintln!("[*] incremental against {p}"); Vhd::create_diff(out, p)?; }
        // Synthetic single-partition disk we lay out ourselves, so 512.
        None => Vhd::create(out, disk_size, 512)?,
    }
    let vhd = Vhd::open(out, true)?;
    vhd.attach(false, false, false)?;
    let disk = vhd.disk_number()?;

    // A differencing disk inherits the parent's partition table -- repartitioning
    // it would orphan the parent's data. Only a full image lays down a new one.
    // ponytail: Windows partitions its own disks correctly; see util::ps.
    let offset: u64 = if parent.is_some() {
        ps(&format!("(Get-Partition -DiskNumber {disk} | Where-Object GptType -eq '{DATA_GUID}').Offset"))?
    } else {
        ps(&format!(
            "Initialize-Disk -Number {disk} -PartitionStyle GPT -Confirm:$false | Out-Null
             (New-Partition -DiskNumber {disk} -UseMaximumSize -GptType '{DATA_GUID}').Offset"
        ))?
    }.parse()?;
    let part_size = ps(&format!(
        "(Get-Partition -DiskNumber {disk} | Where-Object Offset -eq {offset}).Size"
    ))?.parse::<u64>()?;
    if part_size < vol_size {
        return Err(format!("partition {} < volume {}", human(part_size), human(vol_size)).into());
    }

    // A differencing disk inherits the parent's filesystem, not just its
    // partition table, so attaching it makes Windows mount that volume -- and
    // raw access to sectors owned by a mounted volume is denied. Offline is how
    // you get the disk to yourself. Done for full images too: one code path,
    // and nothing can auto-mount underneath us mid-copy.
    ps(&format!(
        "Set-Disk -Number {disk} -IsOffline $true
         Set-Disk -Number {disk} -IsReadOnly $false"
    ))?;

    eprintln!("[*] disk {disk} partition at offset {offset} ({})", human(part_size));
    let dst = Raw::open(&vhd.physical_path()?, true).ctx("open attached vhdx")?;
    Region {
        src: &src, src_off: 0, dst: &dst, dst_off: offset, len: vol_size,
        delta: parent.is_some(), alloc: alloc.as_ref(), tail_slack: TAIL_SLACK,
        label: "volume",
    }.run()?;
    drop(dst);

    vhd.detach()?;
    eprintln!("[+] {out}\n    mount it:  bulkhead mount {out}");
    Ok(())
}

fn cmd_mount(path: &str, rw: bool) -> Res<()> {
    let vhd = Vhd::open(path, rw)?;
    vhd.attach(!rw, true, true)?;
    eprintln!("[+] attached {} ({})", vhd.physical_path()?, if rw { "read-write" } else { "read-only" });
    eprintln!("    it appears in Explorer; detach with:  bulkhead unmount {path}");
    Ok(())
}

fn cmd_unmount(path: &str) -> Res<()> {
    // matches however it was attached
    let vhd = Vhd::open(path, true).or_else(|_| Vhd::open(path, false))?;
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

  bulkhead media <OUT.iso>
      Build bootable WinPE recovery media with bulkhead in it.
      Needs the Windows ADK and its separate WinPE add-on.

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
        ["media", iso] => media::build(iso),
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

    /// Differencing-disk creation needs no elevation, so it is testable here.
    #[test]
    fn differencing() {
        let d = std::env::temp_dir().join("bulkhead-difftest");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let parent = d.join("p.vhdx").to_string_lossy().into_owned();
        let child = d.join("c.vhdx").to_string_lossy().into_owned();

        Vhd::create(&parent, 64 * MB, 512).expect("create parent");
        Vhd::create_diff(&child, &parent).expect("create differencing child");
        // the attach path opens with an explicit mask; that much is testable
        // unelevated, the attach itself is not
        Vhd::open(&child, true).expect("open differencing child for attach");
        assert!(std::fs::metadata(&child).unwrap().len() > 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn runs() {
        let a = vec![0u8; 400];
        // identical -> nothing to write
        assert_eq!(diff_runs(&a, &a, 100), vec![]);
        // no readback -> one run covering everything, so a full copy is 1 write
        assert_eq!(diff_runs(&[], &a, 100), vec![(0, 400)]);

        // one changed byte dirties its grain only
        let mut b = a.clone();
        b[150] = 1;
        assert_eq!(diff_runs(&a, &b, 100), vec![(100, 200)]);

        // adjacent dirty grains coalesce into a single run
        let mut c = a.clone();
        c[150] = 1;
        c[250] = 1;
        assert_eq!(diff_runs(&a, &c, 100), vec![(100, 300)]);

        // a dirty grain at the very end is still flushed, and is short
        let mut d = vec![0u8; 250];
        d[240] = 1;
        assert_eq!(diff_runs(&a[..250], &d, 100), vec![(200, 250)]);
    }

    #[test]
    fn disk_vs_volume() {
        assert_eq!(disk_arg("disk0"), Some(0));
        assert_eq!(disk_arg("Disk12"), Some(12));
        assert_eq!(disk_arg(r"\\.\PhysicalDrive3"), Some(3));
        // a mangled prefix must not still match -- this test previously
        // encoded the same escaping bug as the code it was checking
        assert_eq!(disk_arg(r"\.\PhysicalDrive3"), None);
        assert_eq!(disk_arg("3"), Some(3));
        // volumes must not be mistaken for disks
        assert_eq!(disk_arg("C:"), None);
        assert_eq!(disk_arg("C:\\"), None);
        assert_eq!(disk_arg("disk"), None);
        assert_eq!(disk_arg("diskX"), None);
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
