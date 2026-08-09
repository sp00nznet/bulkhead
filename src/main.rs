//! bulkhead -- block-level backup and recovery for Windows.
//!
//! Images a live volume through a VSS snapshot into a VHDX. VHDX is the point:
//! Windows already mounts one as a drive, already does differencing chains for
//! incrementals, and already boots one. The paid tools charge for those.
mod bitmap;
mod gpt;
mod media;
mod scan;
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

/// Write an image back over a whole disk.
///
/// Destructive and not undoable, so it refuses the disk hosting the running
/// system and asks before writing. From the recovery media the running system
/// is a RAM disk, which is the whole point of having the media.
fn cmd_restore(image: &str, target: &str, yes: bool) -> Res<()> {
    let Some(disk) = disk_arg(target) else {
        return Err(format!(
            "{target:?} is not a disk. restore writes a whole disk (e.g. disk2).\n    \
             For individual files, mount the image and copy them out."
        ).into());
    };

    let sys = ps("(Get-Partition -DriveLetter C -ErrorAction SilentlyContinue).DiskNumber")?;
    if sys.trim() == disk.to_string() {
        return Err(format!(
            "disk {disk} holds the running C:. Boot the recovery media and restore from there."
        ).into());
    }

    let vhd = Vhd::open(image, false)?;
    vhd.attach(true, false, false)?;
    let r = restore_inner(&vhd, disk, yes);
    let _ = vhd.detach();
    r
}

fn restore_inner(vhd: &Vhd, disk: u32, yes: bool) -> Res<()> {
    let src = Raw::open(&vhd.physical_path()?, false).ctx("open image")?;
    let src_size = src.len()?;

    let dst_path = format!(r"\\.\PhysicalDrive{disk}");
    let dst = Raw::open(&dst_path, true).ctx("open target disk")?;
    let dst_size = dst.len()?;
    let sector = dst.sector_size()? as u64;
    if dst_size < src_size {
        return Err(format!(
            "target is {} but the image needs {}", human(dst_size), human(src_size)
        ).into());
    }

    let desc = ps(&format!(
        "Get-Disk -Number {disk} | ForEach-Object {{ \"$($_.FriendlyName) -- $($_.PartitionStyle)\" }}"
    ))?;
    eprintln!("\n[!] This ERASES disk {disk}: {} ({})", desc.trim(), human(dst_size));
    eprintln!("[!] Restoring {} from the image. There is no undo.", human(src_size));
    if !yes {
        eprint!("    Type YES to continue: ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != "YES" {
            return Err("cancelled".into());
        }
    }

    // Offline so Windows releases any volumes it has mounted on the target;
    // sectors owned by a mounted volume cannot be written raw.
    ps(&format!(
        "Set-Disk -Number {disk} -IsOffline $true
         Set-Disk -Number {disk} -IsReadOnly $false"
    ))?;

    Region {
        src: &src, src_off: 0, dst: &dst, dst_off: 0, len: src_size,
        delta: false, alloc: None, tail_slack: 0, label: "disk",
    }.run()?;

    if dst_size > src_size {
        grow_gpt(&dst, dst_size, src_size, sector)?;
    }

    drop(dst);
    ps(&format!("Set-Disk -Number {disk} -IsOffline $false"))?;
    eprintln!("[+] disk {disk} restored");
    Ok(())
}

/// After restoring a smaller image onto a bigger disk, the copied GPT still
/// describes the old disk and its backup table sits stranded mid-disk. Move it
/// to the end so the extra space is addressable and firmware accepts the table.
fn grow_gpt(dst: &Raw, dst_size: u64, src_size: u64, sector: u64) -> Res<()> {
    let mut lba1 = vec![0u8; sector as usize];
    dst.seek(sector)?;
    if dst.read(&mut lba1)? != sector as usize {
        return Err("could not read the restored partition table".into());
    }

    let Some(f) = gpt::relocate(&lba1, dst_size, sector) else {
        eprintln!("[*] not a GPT disk; partition table left exactly as imaged");
        return Ok(());
    };

    // The entry array moves wholesale; only the two headers get rewritten.
    let mut entries = vec![0u8; f.entries_bytes as usize];
    dst.seek(f.source_entries_lba * sector)?;
    dst.read(&mut entries)?;
    dst.seek(f.entries_lba * sector)?;
    dst.write_all(&entries)?;

    let mut sec = vec![0u8; sector as usize];
    sec[..f.primary.len()].copy_from_slice(&f.primary);
    dst.seek(sector)?;
    dst.write_all(&sec)?;

    sec.fill(0);
    sec[..f.backup.len()].copy_from_slice(&f.backup);
    dst.seek(f.last_lba * sector)?;
    dst.write_all(&sec)?;

    eprintln!("[*] GPT extended to {}; {} now unpartitioned and usable",
              human(dst_size), human(dst_size - src_size));
    Ok(())
}

/// Find filesystems whose partition table is gone, and optionally rebuild it.
fn cmd_scan(disk_no: u32, rebuild: bool, yes: bool) -> Res<()> {
    let path = format!(r"\\.\PhysicalDrive{disk_no}");
    let disk = Raw::open(&path, false).ctx("open disk")?;
    let size = disk.len()?;
    eprintln!("[*] scanning {path} ({})", human(size));

    let found = scan::scan(&disk, size)?;
    if found.is_empty() {
        eprintln!("[*] no filesystems found");
        return Ok(());
    }

    let (keep, dropped) = scan::resolve(&found);
    eprintln!("\n{} candidate(s):", found.len());
    for c in &found {
        let mark = if c.report_only { "report-only" }
            else if keep.iter().any(|k| k.start_lba == c.start_lba) { "use" }
            else { "overlaps, skipped" };
        eprintln!("  {:>12}  {:>10}  {:<6} {:<20} [{}]",
                  human(c.start_lba * 512), human(c.bytes()), c.fstype, c.label, mark);
        if !c.note.is_empty() {
            eprintln!("               {}", c.note);
        }
    }
    if !dropped.is_empty() {
        eprintln!("\n[*] {} candidate(s) overlap something larger and were skipped.",
                  dropped.len());
        eprintln!("    Those are usually ghosts of an earlier layout.");
    }

    if !rebuild {
        eprintln!("\n[*] read-only. Add --rebuild to write a partition table for the {} kept.",
                  keep.len());
        return Ok(());
    }
    if keep.is_empty() {
        return Err("nothing usable to rebuild from".into());
    }

    let sys = ps("(Get-Partition -DriveLetter C -ErrorAction SilentlyContinue).DiskNumber")?;
    if sys.trim() == disk_no.to_string() {
        return Err(format!("disk {disk_no} holds the running C:").into());
    }

    // Whatever table is there now gets saved first: the scan is a guess, and
    // being able to put the old one back is the difference between a recovery
    // attempt and a one-way door.
    let backup = std::env::current_dir()?.join(format!("disk{disk_no}-table-backup.bin"));
    let mut head = vec![0u8; 34 * 512];
    disk.seek(0)?;
    disk.read(&mut head)?;
    std::fs::write(&backup, &head)?;
    eprintln!("\n[*] existing table saved to {}", backup.display());

    eprintln!("[!] This REPLACES the partition table on disk {disk_no} with {} entries.",
              keep.len());
    eprintln!("[!] Filesystem contents are not touched, only the table.");
    if !yes {
        eprint!("    Type YES to continue: ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != "YES" {
            return Err("cancelled".into());
        }
    }

    // The table is written directly rather than through New-Partition, which
    // zeroes the first sectors of a partition it creates so that stale
    // filesystem metadata is not picked up. Correct for making a new
    // partition; it destroys the filesystem this command exists to recover.
    // Nothing below writes anywhere except the table's own sectors.
    let sector = disk.sector_size()? as u64;
    let mut parts = Vec::new();
    for c in &keep {
        parts.push(gpt::NewPart {
            type_guid: gpt::guid_bytes(c.gpt_type).ok_or("bad partition type GUID")?,
            unique_guid: new_guid()?,
            start_lba: c.start_lba,
            end_lba: c.end_lba(),
            name: format!("recovered {}", c.fstype),
        });
        eprintln!("[+] {} at {} ({})", c.fstype, human(c.start_lba * 512), human(c.bytes()));
    }
    let table = gpt::build(size, sector, new_guid()?, &parts)
        .ok_or("candidates do not fit a GPT on this disk")?;

    drop(disk);
    ps(&format!("Set-Disk -Number {disk_no} -IsOffline $true -ErrorAction SilentlyContinue"))?;
    let w = Raw::open(&path, true).ctx("open disk for writing")?;
    w.seek(0)?;
    w.write_all(&table.mbr)?;
    w.seek(sector)?;
    let mut hdr = vec![0u8; sector as usize];
    hdr[..table.primary_header.len()].copy_from_slice(&table.primary_header);
    w.write_all(&hdr)?;
    w.seek(table.entries_lba * sector)?;
    w.write_all(&table.entries)?;
    w.seek(table.backup_entries_lba * sector)?;
    w.write_all(&table.entries)?;
    hdr.fill(0);
    hdr[..table.backup_header.len()].copy_from_slice(&table.backup_header);
    w.seek(table.last_lba * sector)?;
    w.write_all(&hdr)?;
    drop(w);

    ps(&format!(
        "Set-Disk -Number {disk_no} -IsOffline $false -ErrorAction SilentlyContinue
         Update-Disk -Number {disk_no} -ErrorAction SilentlyContinue"
    ))?;
    eprintln!("[+] table rebuilt. If it is wrong, restore {}", backup.display());
    Ok(())
}

/// A fresh GUID for a disk or partition, in the byte order GPT stores.
fn new_guid() -> Res<[u8; 16]> {
    let g = windows::core::GUID::new()?;
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&g.data1.to_le_bytes());
    b[4..6].copy_from_slice(&g.data2.to_le_bytes());
    b[6..8].copy_from_slice(&g.data3.to_le_bytes());
    b[8..16].copy_from_slice(&g.data4);
    Ok(b)
}

/// `1048576`, `100MB`, `2GB`, `512K`. Plain numbers are bytes.
fn parse_size(s: &str) -> Option<u64> {
    let t = s.trim().to_ascii_uppercase().replace('B', "");
    let (num, mul) = match t.chars().last()? {
        'K' => (&t[..t.len() - 1], 1u64 << 10),
        'M' => (&t[..t.len() - 1], 1 << 20),
        'G' => (&t[..t.len() - 1], 1 << 30),
        'T' => (&t[..t.len() - 1], 1u64 << 40),
        _ => (t.as_str(), 1),
    };
    num.trim().parse::<u64>().ok()?.checked_mul(mul)
}

/// The whole GPT of a live disk, read once and written back as a unit.
struct Table {
    header: Vec<u8>,
    array: Vec<u8>,
    sector: u64,
}

impl Table {
    fn read(disk: &Raw) -> Res<Table> {
        let sector = disk.sector_size()? as u64;
        let mut lba1 = vec![0u8; sector as usize];
        disk.seek(sector)?;
        disk.read(&mut lba1)?;
        if !gpt::is_gpt(&lba1) {
            return Err("not a GPT disk (MBR disks are not supported yet)".into());
        }
        let header = lba1[..gpt::header_size(&lba1)].to_vec();
        let bytes = gpt::entry_count(&header) * gpt::entry_size(&header);
        let mut array = vec![0u8; bytes.div_ceil(sector as usize) * sector as usize];
        disk.seek(gpt::entry_array_lba(&header) * sector)?;
        disk.read(&mut array)?;
        Ok(Table { header, array, sector })
    }

    /// Write the primary table, its backup, and both headers. Done only after
    /// any data movement has succeeded, so a crash mid-copy leaves a table
    /// that still describes where the data actually is.
    fn write(&mut self, disk: &Raw) -> Res<()> {
        gpt::reseal(&mut self.header, &self.array);

        let mut sec = vec![0u8; self.sector as usize];
        sec[..self.header.len()].copy_from_slice(&self.header);
        disk.seek(self.sector)?;
        disk.write_all(&sec)?;
        disk.seek(gpt::entry_array_lba(&self.header) * self.sector)?;
        disk.write_all(&self.array)?;

        // The backup header describes itself, so it needs its own CRC.
        let last = gpt::alternate_lba(&self.header);
        let entries_lba = last - (self.array.len() as u64 / self.sector);
        let mut backup = self.header.clone();
        gpt::make_backup(&mut backup, last, entries_lba);
        gpt::reseal(&mut backup, &self.array);
        disk.seek(entries_lba * self.sector)?;
        disk.write_all(&self.array)?;
        sec.fill(0);
        sec[..backup.len()].copy_from_slice(&backup);
        disk.seek(last * self.sector)?;
        disk.write_all(&sec)?;
        Ok(())
    }
}

fn cmd_part_list(disk_no: u32) -> Res<()> {
    let disk = Raw::open(&format!(r"\\.\PhysicalDrive{disk_no}"), false).ctx("open disk")?;
    let size = disk.len()?;
    let t = Table::read(&disk)?;
    let parts = gpt::entries(&t.header, &t.array);

    eprintln!("disk {disk_no}: {} ({}-byte sectors)", human(size), t.sector);
    let mut pos = gpt::first_usable(&t.header);
    for p in &parts {
        if p.start_lba > pos {
            eprintln!("     {:>12}  {:>10}  (free)",
                      human(pos * t.sector), human((p.start_lba - pos) * t.sector));
        }
        eprintln!("  {}  {:>12}  {:>10}  {}",
                  p.number, human(p.start_lba * t.sector), human(p.sectors() * t.sector), p.name);
        pos = p.end_lba + 1;
    }
    let end = gpt::last_usable(&t.header);
    if end > pos {
        eprintln!("     {:>12}  {:>10}  (free)",
                  human(pos * t.sector), human((end - pos + 1) * t.sector));
    }
    Ok(())
}

/// Move a partition's data and repoint its table entry.
///
/// Windows cannot do this at any price, and it is what makes "extend into the
/// free space to the left" possible: slide the partition down, then extend it
/// with the native tools.
fn cmd_part_move(disk_no: u32, number: usize, to: u64, yes: bool) -> Res<()> {
    let sys = ps("(Get-Partition -DriveLetter C -ErrorAction SilentlyContinue).DiskNumber")?;
    if sys.trim() == disk_no.to_string() {
        return Err(format!(
            "disk {disk_no} holds the running C:. Move partitions from the recovery media."
        ).into());
    }

    let disk = Raw::open(&format!(r"\\.\PhysicalDrive{disk_no}"), true).ctx("open disk")?;
    let mut t = Table::read(&disk)?;
    let parts = gpt::entries(&t.header, &t.array);
    let me = parts.iter().find(|p| p.number == number)
        .ok_or_else(|| format!("disk {disk_no} has no partition {number}"))?;

    if to % t.sector != 0 {
        return Err(format!("offset must be a multiple of the {}-byte sector", t.sector).into());
    }
    let new_start = to / t.sector;
    let len = me.sectors();
    let new_end = new_start + len - 1;

    if new_start < gpt::first_usable(&t.header) || new_end > gpt::last_usable(&t.header) {
        return Err(format!(
            "{} .. {} is outside the usable area ({} .. {})",
            human(new_start * t.sector), human((new_end + 1) * t.sector),
            human(gpt::first_usable(&t.header) * t.sector),
            human((gpt::last_usable(&t.header) + 1) * t.sector)
        ).into());
    }
    for other in parts.iter().filter(|p| p.number != number) {
        if new_start <= other.end_lba && other.start_lba <= new_end {
            return Err(format!("that would overlap partition {}", other.number).into());
        }
    }
    if new_start == me.start_lba {
        eprintln!("[*] partition {number} is already there");
        return Ok(());
    }

    eprintln!("\n[!] Moving disk {disk_no} partition {number} ({}) from {} to {}",
              human(len * t.sector), human(me.start_lba * t.sector), human(to));
    eprintln!("[!] Interrupting this loses the partition. There is no journal.");
    if !yes {
        eprint!("    Type YES to continue: ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != "YES" {
            return Err("cancelled".into());
        }
    }

    ps(&format!(
        "Set-Disk -Number {disk_no} -IsOffline $true
         Set-Disk -Number {disk_no} -IsReadOnly $false"
    ))?;

    move_bytes(&disk, me.start_lba * t.sector, to, len * t.sector)?;

    // Only now is the table allowed to point at the new location.
    gpt::set_start(&t.header.clone(), &mut t.array, number, new_start)
        .ok_or("could not update the partition entry")?;
    t.write(&disk)?;

    drop(disk);
    ps(&format!("Set-Disk -Number {disk_no} -IsOffline $false"))?;
    eprintln!("[+] partition {number} now starts at {}", human(to));
    Ok(())
}

/// Copy `len` bytes within one device, correct even when the ranges overlap.
///
/// Sliding a partition forward by less than its own length overlaps itself; a
/// forward copy would then read bytes it had already overwritten. Going
/// backwards from the end is the only safe direction in that case.
/// Does moving `len` bytes from `from` to `to` land on top of itself?
fn overlaps_forward(from: u64, to: u64, len: u64) -> bool {
    to > from && to < from + len
}

/// Which slice to copy on the step that has already done `done` bytes.
/// Backwards takes the *last* not-yet-copied chunk, so the read always lands
/// on bytes the write has not reached.
fn step(len: u64, done: u64, chunk: u64, backwards: bool) -> (u64, usize) {
    let n = (len - done).min(chunk);
    let off = if backwards { len - done - n } else { done };
    (off, n as usize)
}

fn move_bytes(disk: &Raw, from: u64, to: u64, len: u64) -> Res<()> {
    let mut buf = vec![0u8; CHUNK];
    let backwards = overlaps_forward(from, to, len);
    let mut done = 0u64;
    let mut last_pct = u64::MAX;

    eprintln!("[*] moving {} {}", human(len), if backwards { "(backwards)" } else { "" });
    while done < len {
        let (off, n) = step(len, done, CHUNK as u64, backwards);
        disk.seek(from + off)?;
        let got = disk.read(&mut buf[..n])?;
        if got != n {
            return Err(format!("short read at {}", from + off).into());
        }
        disk.seek(to + off)?;
        disk.write_all(&buf[..n])?;
        done += n as u64;

        let pct = done * 100 / len;
        if pct != last_pct {
            eprint!("\r  {pct:3}%  {} / {}", human(done), human(len));
            let _ = std::io::stderr().flush();
            last_pct = pct;
        }
    }
    eprintln!("\r  100%  {} / {}      ", human(len), human(len));
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

  bulkhead restore <IMAGE.vhdx> <diskN> [--yes]
      ERASE a disk and write the image back over it. Asks first.
      A bigger target keeps its extra space: the GPT is extended to fit.

  bulkhead part list <diskN>
  bulkhead part move <diskN> <N> --to <OFFSET>   e.g. --to 1MB
      Slide a partition to a new offset. Windows cannot do this at all;
      it is also how you extend into free space that sits to the LEFT.

  bulkhead scan <diskN> [--rebuild] [--yes]
      Find filesystems on a disk whose partition table is lost, and
      optionally write a new table pointing at them. The scan is
      read-only; --rebuild saves the old table first.

  bulkhead media <OUT.iso>
      Build bootable WinPE recovery media with bulkhead in it.
      Needs the Windows ADK and its separate WinPE add-on.

Needs an elevated prompt (raw volume access).";

/// Positional args, with `--flags` and their values removed.
fn positional<'a>(a: &[&'a str]) -> Vec<&'a str> {
    let mut v = Vec::new();
    let mut it = a.iter().copied();
    while let Some(x) = it.next() {
        match x {
            "--from" | "--to" => { it.next(); }
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
        ["restore", img, target] => cmd_restore(img, target, flag("--yes")),
        ["media", iso] => media::build(iso),
        ["scan", d] => disk_arg(d)
            .ok_or_else(|| format!("{d:?} is not a disk").into())
            .and_then(|n| cmd_scan(n, flag("--rebuild"), flag("--yes"))),
        ["part", "list", d] => disk_arg(d)
            .ok_or_else(|| format!("{d:?} is not a disk").into())
            .and_then(cmd_part_list),
        ["part", "move", d, n] => match (disk_arg(d), n.parse(), opt("--to").and_then(parse_size)) {
            (Some(d), Ok(n), Some(to)) => cmd_part_move(d, n, to, flag("--yes")),
            (_, _, None) => Err("part move needs --to <OFFSET>".into()),
            _ => Err(format!("bad disk or partition number: {d:?} {n:?}").into()),
        },
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
        // options swallow their value; bare flags vanish
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
    fn offsets() {
        assert_eq!(parse_size("1048576"), Some(1 << 20));
        assert_eq!(parse_size("100MB"), Some(100 << 20));
        assert_eq!(parse_size("2G"), Some(2 << 30));
        assert_eq!(parse_size(" 512K "), Some(512 << 10));
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size(""), None);
        // must not silently wrap to a tiny offset
        assert_eq!(parse_size("99999999999999T"), None);
    }

    /// Replay a move against a byte array with the same arithmetic the device
    /// path uses. An overlapping forward move that copies front-to-back reads
    /// bytes it has already clobbered, so this is the check that matters.
    fn simulate(from: usize, to: usize, len: usize, chunk: u64) -> (Vec<u8>, Vec<u8>) {
        let original: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        let mut disk = original.clone();
        let backwards = overlaps_forward(from as u64, to as u64, len as u64);
        let mut done = 0u64;
        while done < len as u64 {
            let (off, n) = step(len as u64, done, chunk, backwards);
            let (o, n) = (off as usize, n);
            let piece = disk[from + o..from + o + n].to_vec();
            disk[to + o..to + o + n].copy_from_slice(&piece);
            done += n as u64;
        }
        (disk[to..to + len].to_vec(), original[from..from + len].to_vec())
    }

    #[test]
    fn overlapping_moves_do_not_eat_themselves() {
        // slide forward by less than the length: the dangerous case
        assert!(overlaps_forward(1000, 1200, 500));
        let (moved, want) = simulate(1000, 1200, 500, 128);
        assert_eq!(moved, want, "forward overlap must copy back to front");

        // slide backward: overlapping, but front-to-back is safe
        assert!(!overlaps_forward(1000, 800, 500));
        let (moved, want) = simulate(1000, 800, 500, 128);
        assert_eq!(moved, want);

        // no overlap at all, either direction
        assert!(!overlaps_forward(1000, 2000, 500));
        let (moved, want) = simulate(1000, 2000, 500, 128);
        assert_eq!(moved, want);

        // a length that is not a whole number of chunks
        let (moved, want) = simulate(1000, 1150, 333, 128);
        assert_eq!(moved, want);

        // one chunk covers the whole move
        let (moved, want) = simulate(1000, 1100, 200, 4096);
        assert_eq!(moved, want);
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
