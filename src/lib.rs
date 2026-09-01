//! Block-level backup and recovery for Windows -- the shared core.
//!
//! Images a live volume through a VSS snapshot into a VHDX. VHDX is the point:
//! Windows already mounts one as a drive, already does differencing chains for
//! incrementals, and already boots one. The paid tools charge for those.
//!
//! This is a library with the desktop program on top of it, split by *who runs
//! it*: `bulkhead` is driven by a person at a broken machine. Anything wanting a
//! service account, a credential store or a schedule is deliberately not here.
//! Everything below is shared, and the filesystem readers deliberately are:
//! "get three files off this Linux disk" is a desktop job that file-level
//! restore also needs.
mod bitmap;
mod carve;
mod cert;
mod erase;
mod ext4;
mod gpt;
pub mod gui;
mod hfs;
mod identify;
mod mbr;
pub mod media;
mod ntfs;
mod sanitize;
mod scan;
mod snap;
pub mod util;
mod vhdx;
mod winfsp;
mod xfs;

use std::ffi::c_void;
use std::io::Write as _;
use std::mem::size_of;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_BEGIN, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, ReadFile, SetFilePointerEx, WriteFile,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    DISK_GEOMETRY, FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, GET_LENGTH_INFORMATION,
    IOCTL_DISK_GET_DRIVE_GEOMETRY, IOCTL_DISK_GET_LENGTH_INFO,
};
use windows::core::PCWSTR;

use bitmap::Bitmap;
use snap::Snapshot;
use util::{Ctx, Res, human, ps, wide};
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
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl Raw {
    fn open(path: &str, write: bool) -> Res<Raw> {
        let w = wide(path);
        let access = if write {
            GENERIC_READ | GENERIC_WRITE
        } else {
            GENERIC_READ
        };
        let h = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )?
        };
        Ok(Raw(h))
    }

    /// Send a control code that takes no arguments -- the volume lock family.
    fn fsctl(&self, code: u32) -> Res<()> {
        let mut ret = 0u32;
        unsafe { DeviceIoControl(self.0, code, None, 0, None, 0, Some(&mut ret), None)? };
        Ok(())
    }

    fn len(&self) -> Res<u64> {
        let mut li = GET_LENGTH_INFORMATION::default();
        let mut ret = 0u32;
        let ioctl = unsafe {
            DeviceIoControl(
                self.0,
                IOCTL_DISK_GET_LENGTH_INFO,
                None,
                0,
                Some(&mut li as *mut _ as *mut c_void),
                size_of::<GET_LENGTH_INFORMATION>() as u32,
                Some(&mut ret),
                None,
            )
        };
        if ioctl.is_ok() {
            return Ok(li.Length as u64);
        }
        // A regular file does not answer disk ioctls; ask for its size instead.
        let mut size = 0i64;
        unsafe {
            windows::Win32::Storage::FileSystem::GetFileSizeEx(self.0, &mut size)
                .ctx("length of target")?;
        }
        Ok(size as u64)
    }

    /// Logical bytes-per-sector. A whole-disk image must declare the source's
    /// value or every LBA in the copied GPT points somewhere else.
    fn sector_size(&self) -> Res<u32> {
        let mut g = DISK_GEOMETRY::default();
        let mut ret = 0u32;
        unsafe {
            DeviceIoControl(
                self.0,
                IOCTL_DISK_GET_DRIVE_GEOMETRY,
                None,
                0,
                Some(&mut g as *mut _ as *mut c_void),
                size_of::<DISK_GEOMETRY>() as u32,
                Some(&mut ret),
                None,
            )
            .ctx("IOCTL_DISK_GET_DRIVE_GEOMETRY")?;
        }
        Ok(g.BytesPerSector)
    }

    fn seek(&self, off: u64) -> Res<()> {
        unsafe {
            SetFilePointerEx(self.0, off as i64, None, FILE_BEGIN).ctx("seek")?;
        }
        Ok(())
    }

    fn read(&self, buf: &mut [u8]) -> Res<usize> {
        let mut n = 0u32;
        unsafe {
            ReadFile(self.0, Some(buf), Some(&mut n), None).ctx("read")?;
        }
        Ok(n as usize)
    }

    fn write_all(&self, buf: &[u8]) -> Res<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let mut n = 0u32;
            unsafe {
                WriteFile(self.0, Some(&buf[done..]), Some(&mut n), None).ctx("write")?;
            }
            if n == 0 {
                return Err("short write to target".into());
            }
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
            (false, Some(s)) => {
                runs.push((s, i));
                start = None;
            }
            _ => {}
        }
        if at_end {
            break;
        }
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
        let mut old = if self.delta {
            vec![0u8; CHUNK]
        } else {
            Vec::new()
        };
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
            if self
                .alloc
                .is_some_and(|b| !b.any_allocated(done, done + want as u64))
            {
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
                    return Err(
                        format!("{}: source ended early at {done} of {total}", self.label).into(),
                    );
                }
                eprintln!(
                    "\r
[*] last {} not served by the volume driver; left zeroed",
                    human(total - done)
                );
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
                self.dst
                    .seek(self.dst_off + done + s as u64)
                    .ctx("target")?;
                self.dst.write_all(&buf[s..e]).ctx("target")?;
                written += (e - s) as u64;
            }

            done += n as u64;
        }
        eprintln!("\r  100%  {} / {}      ", human(done), human(total));
        if let Some(b) = self.alloc {
            eprintln!(
                "    {} free space skipped ({} of {} clusters in use)",
                human(skipped),
                b.allocated,
                b.clusters
            );
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
pub fn disk_arg(s: &str) -> Option<u32> {
    let t = s.to_ascii_lowercase();
    let d = t
        .strip_prefix(r"\\.\physicaldrive")
        .or_else(|| t.strip_prefix("disk"))
        .unwrap_or(&t);
    if d.is_empty() {
        return None;
    }
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
        if f.len() < 2 {
            continue;
        }
        v.push(Part {
            offset: f[0].parse()?,
            size: f[1].parse()?,
            letter: f
                .get(2)
                .and_then(|s| s.chars().next())
                .filter(|c| c.is_ascii_alphabetic()),
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
    eprintln!(
        "[*] source {phys_path} ({}, {sector}-byte sectors)",
        human(disk_size)
    );

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
                // Not a failure worth stopping for: an unsnapshottable volume
                // is copied raw, which is exactly right for the FAT and
                // removable volumes VSS declines.
                Err(e) => eprintln!(
                    "[!] {l}: {e}
    copying it raw instead"
                ),
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

fn image_disk_inner(
    phys: &Raw,
    disk_size: u64,
    sector: u32,
    parts: &[Part],
    shadows: &[(usize, Snapshot)],
    out: &str,
    parent: Option<&str>,
) -> Res<()> {
    match parent {
        Some(p) => {
            eprintln!("[*] incremental against {p}");
            Vhd::create_diff(out, p)?;
        }
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
        Region {
            src: phys,
            src_off: off,
            dst: &dst,
            dst_off: off,
            len,
            delta,
            alloc: None,
            tail_slack: 0,
            label,
        }
        .run()
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
                Region {
                    src: &src,
                    src_off: 0,
                    dst: &dst,
                    dst_off: p.offset,
                    len: vol_size,
                    delta,
                    alloc: alloc.as_ref(),
                    tail_slack: TAIL_SLACK,
                    label: &label,
                }
                .run()?;
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
    eprintln!(
        "[+] {out}
    mount it:  bulkhead mount {out}"
    );
    Ok(())
}

pub fn cmd_image(volume: &str, out: &str, use_vss: bool, parent: Option<&str>) -> Res<()> {
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
        None => {
            format!(
                r"\\.\{}",
                volume.trim_end_matches('\\').trim_end_matches(':')
            ) + ":"
        }
    };
    let r = image_inner(&src_path, out, parent);
    if let Some(s) = &shadow {
        eprintln!("[*] releasing snapshot");
        if let Err(e) = s.delete() {
            eprintln!("[!] snapshot {} not released: {e}", s.id);
        }
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
        Some(b) => eprintln!(
            "[*] {} in use of {} ({}-byte clusters)",
            human(b.allocated * b.cluster),
            human(vol_size),
            b.cluster
        ),
        None => eprintln!("[*] no allocation bitmap; imaging every sector"),
    }

    // Slack for 1 MiB alignment, the backup GPT at the tail, and the Microsoft
    // Reserved partition Initialize-Disk inserts (16 MiB under 16 GB, 32 MiB
    // over). Over-reserving is free: the VHDX is dynamic.
    let disk_size = (vol_size + 40 * MB).div_ceil(MB) * MB;
    match parent {
        Some(p) => {
            eprintln!("[*] incremental against {p}");
            Vhd::create_diff(out, p)?;
        }
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
        ps(&format!(
            "(Get-Partition -DiskNumber {disk} | Where-Object GptType -eq '{DATA_GUID}').Offset"
        ))?
    } else {
        ps(&format!(
            "Initialize-Disk -Number {disk} -PartitionStyle GPT -Confirm:$false | Out-Null
             (New-Partition -DiskNumber {disk} -UseMaximumSize -GptType '{DATA_GUID}').Offset"
        ))?
    }
    .parse()?;
    let part_size = ps(&format!(
        "(Get-Partition -DiskNumber {disk} | Where-Object Offset -eq {offset}).Size"
    ))?
    .parse::<u64>()?;
    if part_size < vol_size {
        return Err(format!(
            "partition {} < volume {}",
            human(part_size),
            human(vol_size)
        )
        .into());
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

    eprintln!(
        "[*] disk {disk} partition at offset {offset} ({})",
        human(part_size)
    );
    let dst = Raw::open(&vhd.physical_path()?, true).ctx("open attached vhdx")?;
    Region {
        src: &src,
        src_off: 0,
        dst: &dst,
        dst_off: offset,
        len: vol_size,
        delta: parent.is_some(),
        alloc: alloc.as_ref(),
        tail_slack: TAIL_SLACK,
        label: "volume",
    }
    .run()?;
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
pub fn cmd_restore(image: &str, target: &str, yes: bool) -> Res<()> {
    let Some(disk) = disk_arg(target) else {
        return Err(format!(
            "{target:?} is not a disk. restore writes a whole disk (e.g. disk2).\n    \
             For individual files, mount the image and copy them out."
        )
        .into());
    };

    let sys = ps("(Get-Partition -DriveLetter C -ErrorAction SilentlyContinue).DiskNumber")?;
    if sys.trim() == disk.to_string() {
        return Err(format!(
            "disk {disk} holds the running C:. Boot the recovery media and restore from there."
        )
        .into());
    }

    let vhd = Vhd::open(image, false)?;
    vhd.attach(true, false, false)?;
    let r = restore_inner(&vhd, disk, yes);
    // Detach before the disk is offered to Windows again. The restored disk
    // carries the image's disk GUID, so while the image is still attached the
    // two collide, and Windows resolves that by rewriting the target's
    // partition table: fresh v1 GUIDs built from the NIC's MAC, and the
    // entries compacted. A Debian restore came back with new PARTUUIDs and its
    // partitions renumbered from 1/14/15 to 1/2/3, so `root=PARTUUID=...`
    // pointed at nothing and the restored system dropped to an initramfs
    // shell.
    let _ = vhd.detach();
    r?;

    // And leave it offline. Bringing it online invites the same rewrite from
    // any other attached disk carrying that GUID -- the original, most
    // obviously, which is exactly what a restore-and-verify has plugged in.
    // A restored disk is for booting or for reading deliberately, so make
    // that step the operator's.
    eprintln!("[*] disk {disk} is offline, so nothing can rewrite its partition table.");
    eprintln!("    Boot it, or bring it online with:  Set-Disk -Number {disk} -IsOffline $false");
    Ok(())
}

/// Get Windows to let go of a disk so its sectors can be written raw.
///
/// Sectors owned by a mounted volume cannot be written through the physical
/// drive; the write is refused with access denied. For a fixed disk, taking it
/// offline is enough. **Removable media cannot be taken offline at all** --
/// card readers and USB sticks refuse it -- so there the volumes have to be
/// locked and dismounted one at a time instead.
///
/// The returned handles are the lock. It lasts exactly as long as they do, so
/// they must stay alive for the whole write.
fn release_disk(disk: u32) -> Res<Vec<Raw>> {
    // Allowed to fail: removable media has no offline state. The volume locks
    // below are what actually matter there, so this is not an error.
    let offlined = ps(&format!("Set-Disk -Number {disk} -IsOffline $true")).is_ok();
    let _ = ps(&format!(
        "Set-Disk -Number {disk} -IsReadOnly $false -ErrorAction SilentlyContinue"
    ));

    let letters = ps(&format!(
        "(Get-Partition -DiskNumber {disk} -ErrorAction SilentlyContinue).DriveLetter -join ''"
    ))?;
    let mut held = Vec::new();
    for c in letters.chars().filter(|c| c.is_ascii_alphabetic()) {
        let v = Raw::open(&format!(r"\\.\{c}:"), true).ctx("open volume to lock")?;
        // Lock first: it fails if anything else has the volume open, which is
        // worth hearing about before a wipe rather than halfway through one.
        v.fsctl(FSCTL_LOCK_VOLUME).map_err(|e| {
            format!(
                "{c}: is in use and could not be locked ({e}). \
                                  Close anything reading it and try again"
            )
        })?;
        v.fsctl(FSCTL_DISMOUNT_VOLUME).ctx("dismount volume")?;
        eprintln!("[*] {c}: locked and dismounted");
        held.push(v);
    }
    if !offlined && held.is_empty() {
        eprintln!("[!] the disk would not go offline and has no volumes to lock;");
        eprintln!("    if the write is refused, something else still holds it");
    }
    Ok(held)
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
            "target is {} but the image needs {}",
            human(dst_size),
            human(src_size)
        )
        .into());
    }

    let desc = ps(&format!(
        "Get-Disk -Number {disk} | ForEach-Object {{ \"$($_.FriendlyName) -- $($_.PartitionStyle)\" }}"
    ))?;
    eprintln!(
        "\n[!] This ERASES disk {disk}: {} ({})",
        desc.trim(),
        human(dst_size)
    );
    eprintln!(
        "[!] Restoring {} from the image. There is no undo.",
        human(src_size)
    );
    if !yes {
        eprint!("    Type YES to continue: ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != "YES" {
            return Err("cancelled".into());
        }
    }

    let locks = release_disk(disk)?;

    Region {
        src: &src,
        src_off: 0,
        dst: &dst,
        dst_off: 0,
        len: src_size,
        delta: false,
        alloc: None,
        tail_slack: 0,
        label: "disk",
    }
    .run()?;

    if dst_size > src_size {
        grow_gpt(&dst, dst_size, src_size, sector)?;
    }

    drop(locks);
    drop(dst);
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

    eprintln!(
        "[*] GPT extended to {}; {} now unpartitioned and usable",
        human(dst_size),
        human(dst_size - src_size)
    );
    Ok(())
}

/// Find filesystems whose partition table is gone, and optionally rebuild it.
pub fn cmd_scan(disk_no: u32, rebuild: bool, yes: bool) -> Res<()> {
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
        let mark = if c.report_only {
            "report-only"
        } else if keep.iter().any(|k| k.start_lba == c.start_lba) {
            "use"
        } else {
            "overlaps, skipped"
        };
        eprintln!(
            "  {:>12}  {:>10}  {:<6} {:<20} [{}]",
            human(c.start_lba * 512),
            human(c.bytes()),
            c.fstype,
            c.label,
            mark
        );
        if !c.note.is_empty() {
            eprintln!("               {}", c.note);
        }
    }
    if !dropped.is_empty() {
        eprintln!(
            "\n[*] {} candidate(s) overlap something larger and were skipped.",
            dropped.len()
        );
        eprintln!("    Those are usually ghosts of an earlier layout.");
    }

    if !rebuild {
        eprintln!(
            "\n[*] read-only. Add --rebuild to write a partition table for the {} kept.",
            keep.len()
        );
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
    save_table(&disk, size, &backup)?;
    eprintln!("\n[*] existing table saved to {}", backup.display());
    eprintln!(
        "    put it back with:  bulkhead undo disk{disk_no} {}",
        backup.display()
    );

    eprintln!(
        "[!] This REPLACES the partition table on disk {disk_no} with {} entries.",
        keep.len()
    );
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
        eprintln!(
            "[+] {} at {} ({})",
            c.fstype,
            human(c.start_lba * 512),
            human(c.bytes())
        );
    }
    let table = gpt::build(size, sector, new_guid()?, &parts)
        .ok_or("candidates do not fit a GPT on this disk")?;

    drop(disk);
    ps(&format!(
        "Set-Disk -Number {disk_no} -IsOffline $true -ErrorAction SilentlyContinue"
    ))?;
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
    eprintln!(
        "[+] table rebuilt. If it is wrong:  bulkhead undo disk{disk_no} {}",
        backup.display()
    );
    Ok(())
}

/// How much of each end of a disk a table backup covers. A megabyte at each
/// end takes in the protective MBR, both GPT copies and an MBR's worth of
/// slack, without needing to know the sector size to read it back.
const TABLE_BACKUP: u64 = MB;

fn save_table(disk: &Raw, size: u64, path: &std::path::Path) -> Res<()> {
    let mut buf = vec![0u8; (TABLE_BACKUP * 2) as usize];
    disk.seek(0)?;
    disk.read(&mut buf[..TABLE_BACKUP as usize])?;
    disk.seek(size - TABLE_BACKUP)?;
    disk.read(&mut buf[TABLE_BACKUP as usize..])?;
    std::fs::write(path, &buf)?;
    Ok(())
}

/// Put a saved partition table back.
pub fn cmd_undo(disk_no: u32, file: &str, yes: bool) -> Res<()> {
    let saved = std::fs::read(file)?;
    if saved.len() as u64 != TABLE_BACKUP * 2 {
        return Err(format!(
            "{file} is {} bytes; a table backup is {}",
            saved.len(),
            TABLE_BACKUP * 2
        )
        .into());
    }
    let sys = ps("(Get-Partition -DriveLetter C -ErrorAction SilentlyContinue).DiskNumber")?;
    if sys.trim() == disk_no.to_string() {
        return Err(format!("disk {disk_no} holds the running C:").into());
    }

    let path = format!(r"\\.\PhysicalDrive{disk_no}");
    let size = Raw::open(&path, false).ctx("open disk")?.len()?;
    eprintln!("\n[!] This replaces the partition table on disk {disk_no} with {file}.");
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
        "Set-Disk -Number {disk_no} -IsOffline $true -ErrorAction SilentlyContinue"
    ))?;
    let w = Raw::open(&path, true).ctx("open disk for writing")?;
    w.seek(0)?;
    w.write_all(&saved[..TABLE_BACKUP as usize])?;
    w.seek(size - TABLE_BACKUP)?;
    w.write_all(&saved[TABLE_BACKUP as usize..])?;
    drop(w);
    ps(&format!(
        "Set-Disk -Number {disk_no} -IsOffline $false -ErrorAction SilentlyContinue
         Update-Disk -Number {disk_no} -ErrorAction SilentlyContinue"
    ))?;
    eprintln!("[+] disk {disk_no} table restored from {file}");
    Ok(())
}

/// Recover deleted files from an NTFS volume.
pub fn cmd_undelete(target: &str, at: Option<u64>, out_dir: &str, limit: usize) -> Res<()> {
    let (path, base) = match disk_arg(target) {
        Some(n) => (format!(r"\\.\PhysicalDrive{n}"), at.unwrap_or(0)),
        None => {
            if at.is_some() {
                return Err("--at applies to a disk, not a volume".into());
            }
            (
                format!(
                    r"\\.\{}",
                    target.trim_end_matches('\\').trim_end_matches(':')
                ) + ":",
                0,
            )
        }
    };
    let disk = Raw::open(&path, false).ctx("open volume")?;
    let fs = ntfs::Ntfs::open(&disk, base)?;
    eprintln!(
        "[*] {path} at {}: {}-byte clusters, {} MFT records",
        human(base),
        fs.cluster,
        fs.records()
    );

    let found = fs.deleted(limit, |n, total| {
        eprint!("\r  {:3}%  {n} / {total} records", n * 100 / total.max(1));
        let _ = std::io::stderr().flush();
    });
    eprintln!(
        "\r  {} deleted file(s) with recoverable data          ",
        found.len()
    );
    if found.is_empty() {
        return Ok(());
    }

    std::fs::create_dir_all(out_dir)?;
    let mut ok = 0u64;
    let mut bytes = 0u64;
    for (i, d) in found.iter().enumerate() {
        // Names come from a deleted record and are not to be trusted with a
        // path: strip anything that could climb out of the output directory.
        let safe: String = d
            .name
            .chars()
            .map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
            .collect();
        let dest = std::path::Path::new(out_dir).join(format!("{:04}_{}", i, safe));
        match fs.read_file(d) {
            Ok(data) => {
                let partial = (data.len() as u64) < d.size;
                std::fs::write(&dest, &data)?;
                ok += 1;
                bytes += data.len() as u64;
                eprintln!(
                    "  {} ({}){}",
                    safe,
                    human(d.size),
                    if partial {
                        format!(" -- PARTIAL, only {} readable", human(data.len() as u64))
                    } else {
                        String::new()
                    }
                );
            }
            Err(e) => eprintln!("  [!] {safe}: {e}"),
        }
    }
    eprintln!("[+] {ok} file(s), {} written to {out_dir}", human(bytes));
    eprintln!("    Deleted clusters are free space; anything written to this");
    eprintln!("    volume since may be sitting in them. Check the contents.");
    Ok(())
}

/// Carve files out of raw bytes, for when no filesystem survives.
pub fn cmd_carve(target: &str, out_dir: &str, limit: usize) -> Res<()> {
    // The same opener as ls, cp and undelete. carve had its own copy that
    // only knew disks and volumes, so an image file -- the way every other
    // reader gets tested without the media -- failed with a bare 0x8007007B.
    let (disk, _, name) = open_target(target, None)?;
    let size = disk.len()?;
    eprintln!("[*] carving {name} ({})", human(size));
    let n = carve::carve(&disk, size, std::path::Path::new(out_dir), limit)?;
    eprintln!("[+] {n} file(s) written to {out_dir}");
    if n > 0 {
        eprintln!("    Carved files have no names and are one contiguous stretch each,");
        eprintln!("    so anything the filesystem fragmented comes back truncated.");
    }
    Ok(())
}

/// Open a target as a raw device plus a byte offset into it.
///
/// A plain file works too, which is how these get tested against filesystem
/// images without needing the media.
fn open_target(target: &str, at: Option<u64>) -> Res<(Raw, u64, String)> {
    if std::path::Path::new(target).is_file() {
        let d = Raw::open(target, false).ctx("open image file")?;
        return Ok((d, at.unwrap_or(0), target.to_string()));
    }
    match disk_arg(target) {
        Some(n) => {
            let p = format!(r"\\.\PhysicalDrive{n}");
            let d = Raw::open(&p, false).ctx("open disk")?;
            Ok((d, at.unwrap_or(0), p))
        }
        None => {
            let p = format!(
                r"\\.\{}",
                target.trim_end_matches('\\').trim_end_matches(':')
            ) + ":";
            let d = Raw::open(&p, false).ctx("open volume")?;
            Ok((d, 0, p))
        }
    }
}

/// A directory entry, whichever filesystem it came from.
struct Entry {
    inode: u64,
    name: String,
    is_dir: bool,
}

/// The filesystems Windows will not mount, behind one interface.
///
/// An enum rather than a trait: there are a handful of these, they are all
/// read-only, and the methods are the same four every time.
enum Fs<'a> {
    Ext(ext4::Ext<'a>),
    Xfs(xfs::Xfs<'a>),
    Hfs(hfs::Hfs<'a>),
}

impl<'a> Fs<'a> {
    fn open(disk: &'a Raw, base: u64) -> Res<Fs<'a>> {
        if let Ok(e) = ext4::Ext::open(disk, base) {
            return Ok(Fs::Ext(e));
        }
        if let Ok(x) = xfs::Xfs::open(disk, base) {
            return Ok(Fs::Xfs(x));
        }
        match hfs::Hfs::open(disk, base) {
            Ok(h) => Ok(Fs::Hfs(h)),
            Err(_) => Err(format!(
                "no ext2/3/4, XFS or HFS+ volume at {}.
                     NTFS and FAT are readable by Windows itself; for those use                  Explorer, or `bulkhead undelete` for deleted files.",
                human(base)
            ).into()),
        }
    }

    fn describe(&self) -> String {
        match self {
            Fs::Ext(e) => format!(
                "ext2/3/4, {}{}",
                human(e.blocks * e.block_size),
                if e.label.is_empty() {
                    String::new()
                } else {
                    format!(", label {:?}", e.label)
                }
            ),
            Fs::Xfs(x) => format!(
                "XFS, {}{}",
                human(x.blocks * x.blocksize),
                if x.label.is_empty() {
                    String::new()
                } else {
                    format!(", label {:?}", x.label)
                }
            ),
            Fs::Hfs(h) => format!(
                "HFS+{}, {}",
                if h.case_sensitive {
                    "X (case-sensitive)"
                } else {
                    ""
                },
                human(h.blocks as u64 * h.block_size)
            ),
        }
    }

    fn resolve(&self, path: &str) -> Res<(u64, bool)> {
        match self {
            Fs::Ext(e) => e.resolve(path),
            Fs::Xfs(x) => x.resolve(path),
            Fs::Hfs(h) => h.resolve(path).map(|(id, d)| (id as u64, d)),
        }
    }

    fn read_dir(&self, ino: u64) -> Res<Vec<Entry>> {
        Ok(match self {
            Fs::Ext(e) => e
                .read_dir(ino)?
                .into_iter()
                .map(|d| Entry {
                    inode: d.inode,
                    name: d.name,
                    is_dir: d.is_dir,
                })
                .collect(),
            Fs::Xfs(x) => x
                .read_dir(ino)?
                .into_iter()
                .map(|d| Entry {
                    inode: d.inode,
                    name: d.name,
                    is_dir: d.is_dir,
                })
                .collect(),
            Fs::Hfs(h) => h
                .read_dir(ino as u32)?
                .into_iter()
                .map(|d| Entry {
                    inode: d.id as u64,
                    name: d.name,
                    is_dir: d.is_dir,
                })
                .collect(),
        })
    }

    fn read_file(&self, ino: u64) -> Res<Vec<u8>> {
        match self {
            Fs::Ext(e) => e.read_file(ino),
            Fs::Xfs(x) => x.read_file(ino),
            Fs::Hfs(h) => h.read_file(ino as u32),
        }
    }

    fn label(&self) -> String {
        match self {
            Fs::Ext(e) => e.label.clone(),
            Fs::Xfs(x) => x.label.clone(),
            Fs::Hfs(_) => String::new(),
        }
    }

    fn total(&self) -> u64 {
        match self {
            Fs::Ext(e) => e.blocks * e.block_size,
            Fs::Xfs(x) => x.blocks * x.blocksize,
            Fs::Hfs(h) => h.blocks as u64 * h.block_size,
        }
    }

    fn size_of(&self, ino: u64) -> Res<u64> {
        match self {
            Fs::Ext(e) => e.size_of(ino),
            Fs::Xfs(x) => x.size_of(ino),
            Fs::Hfs(h) => h.size_of(ino as u32),
        }
    }
}

/// Report what a drive can do about erasing itself. Read-only.
pub fn cmd_erase_info(target: &str) -> Res<()> {
    let Some(n) = disk_arg(target) else {
        return Err(format!("{target:?} is not a disk; erase works on whole drives").into());
    };
    let path = format!(r"\\.\PhysicalDrive{n}");
    // Pass-through IOCTLs need write access on the handle even though this
    // reads only. Nothing here issues a command that changes the drive.
    let disk = Raw::open(&path, true)
        .or_else(|_| Raw::open(&path, false))
        .ctx("open disk")?;
    eprintln!("[*] {path} ({})", human(disk.len().unwrap_or(0)));

    let (caps, notes) = erase::capabilities(&disk);
    for l in erase::report(&caps) {
        eprintln!("  {l}");
    }
    let methods = caps.methods();
    if !methods.is_empty() {
        eprintln!(
            "\r
[*] usable: {}",
            methods.join(", ")
        );
    } else if caps.answered {
        eprintln!(
            "\r
[!] no usable erase command on this drive"
        );
    } else {
        eprintln!(
            "\r
[!] erase capability unknown -- the drive did not answer"
        );
    }
    // Ask the drive for its sanitize status. That command changes nothing, but
    // it rides the same pass-through, the same task-file split and the same
    // 48-bit flag as the sanitize that erases the drive -- so if this answers,
    // the destructive one will reach the drive too. Better to learn that here
    // than after typing a serial number.
    if caps.ata_sanitize {
        match sanitize::status(&disk) {
            Ok((true, _)) => {
                eprintln!("[+] sanitize commands reach this drive; none in progress")
            }
            Ok((false, pct)) => {
                eprintln!("[!] a sanitize is ALREADY RUNNING on this drive ({pct}%)")
            }
            Err(e) => {
                eprintln!("[!] the drive advertises sanitize but would not answer a");
                eprintln!("    status query: {e}");
                eprintln!("    an erase would not reach it either. Usually the storage");
                eprintln!("    driver or a USB bridge refusing to pass the command on.");
            }
        }
    }
    for b in caps.blockers() {
        eprintln!("[!] {b}");
    }
    for n in notes {
        eprintln!("[*] {n}");
    }
    Ok(())
}

/// Overwrite every sector of a drive, then check that it took.
///
/// This is the fallback, not the good option. A firmware sanitize tells the
/// drive to erase itself, including the blocks it has quietly remapped out of
/// service; an overwrite can only reach what the drive currently maps. On
/// flash -- SSDs, SD cards, USB sticks -- wear levelling means some old data
/// can survive in spare blocks that no write will ever land on. Say so rather
/// than implying otherwise.
pub fn cmd_erase(target: &str, method: Option<&str>, yes: bool, cert_to: Option<&str>) -> Res<()> {
    let Some(n) = disk_arg(target) else {
        return Err(format!("{target:?} is not a disk; erase works on whole drives").into());
    };
    let sys = ps("(Get-Partition -DriveLetter C -ErrorAction SilentlyContinue).DiskNumber")?;
    if sys.trim() == n.to_string() {
        return Err(format!("disk {n} holds the running C:").into());
    }

    let path = format!(r"\\.\PhysicalDrive{n}");
    let disk = Raw::open(&path, true).ctx("open disk for writing")?;
    let size = disk.len()?;
    let sector = disk.sector_size()? as u64;
    let (caps, _) = erase::capabilities(&disk);

    eprintln!("[*] {path}");
    for l in erase::report(&caps) {
        eprintln!("  {l}");
    }

    let firmware = caps.methods();
    let chosen = match method {
        Some(m) => m,
        None if !firmware.is_empty() => firmware[0],
        None => {
            return Err("no firmware erase available on this drive.
                     Add --method overwrite to write over every sector instead,                  understanding that on flash it cannot reach remapped blocks.".to_string().into());
        }
    };
    let kind = sanitize::Kind::parse(chosen);
    if kind.is_none() && chosen != "overwrite" {
        return Err(format!(
            "{chosen} is not implemented yet. Available: overwrite{}{}",
            if firmware.iter().any(|m| sanitize::Kind::parse(m).is_some()) {
                ", "
            } else {
                ""
            },
            firmware
                .iter()
                .filter(|m| sanitize::Kind::parse(m).is_some())
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    }
    // Asking for a sanitize the drive never advertised is worth stopping on:
    // it will be refused anyway, and a clear message beats an ATA error code.
    if kind.is_some() && !firmware.contains(&chosen) {
        return Err(format!(
            "this drive does not advertise {chosen}. It advertises: {}",
            if firmware.is_empty() {
                "nothing".into()
            } else {
                firmware.join(", ")
            }
        )
        .into());
    }

    // Sample before, so afterwards there is something to compare against.
    let points = erase::sample_points(size, 32, sector);
    let mut before = Vec::new();
    for &at in &points {
        let mut b = vec![0u8; sector as usize];
        disk.seek(at)?;
        let _ = disk.read(&mut b);
        before.push(b);
    }
    let had_data = before.iter().any(|b| !erase::is_pattern(b, 0));

    eprintln!(
        "\r
[!] This ERASES disk {n}: {} ({}), serial {}",
        caps.model,
        human(size),
        caps.serial
    );
    match kind {
        Some(k) => eprintln!("[!] The drive erases itself ({chosen}, {k:?}). There is no undo."),
        None => {
            eprintln!("[!] Every sector is overwritten with zeros. There is no undo.");
            eprintln!("[!] An overwrite cannot reach blocks the drive has remapped out");
            eprintln!("    of service, so it is not equivalent to a firmware sanitize.");
            if !firmware.is_empty() {
                eprintln!(
                    "[!] This drive DOES offer {} -- prefer that.",
                    firmware.join(", ")
                );
            }
        }
    }
    if !yes {
        // The serial, not "YES": it cannot be typed by reflex, and it forces a
        // look at which drive this actually is.
        eprint!("    Type the serial ({}) to continue: ", caps.serial);
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != caps.serial.trim() {
            return Err("cancelled".into());
        }
    }

    let locks = release_disk(n)?;
    let started = std::time::Instant::now();

    match kind {
        // The drive does the work. All we do is start it and watch.
        Some(k) => {
            sanitize::start(&disk, k)?;
            eprintln!("[*] the drive accepted the command and is erasing itself");
            let mut last = u8::MAX;
            loop {
                let (done, pct) = sanitize::status(&disk)?;
                if done {
                    break;
                }
                if pct != last {
                    eprint!("\r  {pct:3}%");
                    let _ = std::io::stderr().flush();
                    last = pct;
                }
                // ponytail: fixed poll. A crypto scramble finishes in about a
                // second, a block erase in minutes; nothing here needs finer
                // reporting than that.
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            eprintln!("\r  100%      ");
        }
        None => {
            let zeros = vec![0u8; CHUNK];
            let mut done = 0u64;
            let mut last_pct = u64::MAX;
            disk.seek(0)?;
            while done < size {
                let want = ((size - done) as usize).min(CHUNK);
                disk.write_all(&zeros[..want]).ctx("overwrite")?;
                done += want as u64;
                let pct = done * 100 / size;
                if pct != last_pct {
                    eprint!("\r  {pct:3}%  {} / {}", human(done), human(size));
                    let _ = std::io::stderr().flush();
                    last_pct = pct;
                }
            }
            eprintln!("\r  100%  {} / {}      ", human(size), human(size));
        }
    }

    // Read it back. An erase nobody checked is a claim, not a result.
    //
    // What counts as erased depends on the method. An overwrite or a block
    // erase leaves the media blank. A crypto scramble does not: it throws away
    // the key, so the sectors still read as dense ciphertext. Checking those
    // for blankness would fail a successful erase, so the test is that the
    // bytes changed -- which is all that can honestly be observed from here.
    let seconds = started.elapsed().as_secs();
    let crypto = kind == Some(sanitize::Kind::CryptoScramble);
    let mut bad = 0;
    let mut changed = 0;
    let mut samples = Vec::new();
    for (i, &at) in points.iter().enumerate() {
        let mut b = vec![0u8; sector as usize];
        disk.seek(at)?;
        let got = disk.read(&mut b).unwrap_or(0);
        if got != sector as usize {
            eprintln!("[!] {} could not be read back", human(at));
            bad += 1;
            samples.push(cert::Point {
                at,
                before: hex16(&before[i]),
                after: "unreadable".into(),
                ok: false,
            });
            continue;
        }
        if b != before[i] {
            changed += 1;
        }
        let ok = if crypto {
            // Only the old contents disappearing is checkable here.
            b != before[i] || erase::is_pattern(&before[i], 0)
        } else {
            // 0xFF as well as 0x00: erased flash reads as ones on some drives.
            erase::is_pattern(&b, 0) || erase::is_pattern(&b, 0xFF)
        };
        if !ok {
            bad += 1;
            if crypto {
                eprintln!("[!] {} is unchanged: {}", human(at), hex16(&b));
            } else {
                eprintln!("[!] {} still holds data: {}", human(at), hex16(&b));
            }
        }
        samples.push(cert::Point {
            at,
            before: hex16(&before[i]),
            after: hex16(&b),
            ok,
        });
    }
    drop(locks);
    drop(disk);
    ps(&format!(
        "Set-Disk -Number {n} -IsOffline $false -ErrorAction SilentlyContinue"
    ))?;

    // The certificate is written whether or not it passed. One that only
    // exists on success is one that lies by omission.
    if let Some(path) = cert_to {
        let (host, operator) = cert::who();
        let doc = cert::Cert {
            when: cert::utc_now(),
            host,
            operator,
            tool: concat!("bulkhead ", env!("CARGO_PKG_VERSION")).into(),
            disk: n,
            model: caps.model.clone(),
            serial: caps.serial.clone(),
            firmware: caps.firmware.clone(),
            bus: caps.bus.map(|b| b.name()).unwrap_or("unknown".into()),
            size,
            method: chosen.to_string(),
            seconds,
            had_data,
            points: samples,
            passed: bad == 0,
            caveats: erase_caveats(chosen, crypto, had_data),
        };
        doc.write(path)?;
        eprintln!("[+] certificate written to {path}");
    }

    if bad > 0 {
        return Err(format!("{bad} of {} sampled points did not verify", points.len()).into());
    }
    let total = points.len();
    if crypto {
        eprintln!("[+] {changed} of {total} sample points changed");
        eprintln!("[*] a crypto scramble leaves ciphertext behind, not blank sectors.");
        eprintln!("    What is verified here is that the old contents are gone; that");
        eprintln!("    the key was destroyed is the drive's claim, not ours.");
        return Ok(());
    }
    eprintln!("[+] {total} sample points across the drive read back blank");
    if had_data {
        eprintln!("[+] those points held data before, and do not now");
    } else {
        eprintln!("[*] the sampled points were already blank beforehand, so this");
        eprintln!("    run proves the erase succeeded, not that anything was removed");
    }
    Ok(())
}

fn hex16(b: &[u8]) -> String {
    b.iter().take(16).map(|x| format!("{x:02x}")).collect()
}

/// The limits of what the run just did, in the words that go on the paper.
///
/// These are the same things `erase` prints. They exist twice on purpose: a
/// certificate is read months later by someone who never saw the terminal,
/// and the caveats are the half that decides whether the drive can leave the
/// building.
fn erase_caveats(method: &str, crypto: bool, had_data: bool) -> Vec<String> {
    let mut v = vec![
        "Verification is by sampling. Points spread across the drive were read \
         back, including its first and last sectors; the whole surface was not \
         re-read."
            .to_string(),
    ];
    if method == "overwrite" {
        v.push(
            "A host overwrite reaches only the blocks the drive currently maps. \
             Sectors retired by the firmware over the drive's life are not \
             reachable from outside it, and nothing observable from the host can \
             say whether any exist."
                .into(),
        );
        v.push(
            "On flash -- SSDs, SD cards, USB sticks -- wear levelling can leave \
             old contents in spare blocks that no write will ever land on. NIST \
             SP 800-88 classes this as Clear, not Purge; only a firmware \
             sanitize supports the stronger claim."
                .into(),
        );
    } else {
        v.push(
            "The drive reported that it completed the command. That the firmware \
             did what the command specifies is the drive's claim, not something \
             this tool observed."
                .into(),
        );
    }
    if crypto {
        v.push(
            "A cryptographic erase discards the key rather than clearing the \
             media, so the sectors still read as dense ciphertext. What is \
             verified here is that the previous contents are gone; that the key \
             was destroyed is the drive's claim."
                .into(),
        );
    }
    if !had_data {
        v.push(
            "The sampled points were already blank before the erase, so this run \
             shows the command succeeded rather than that data was removed."
                .into(),
        );
    }
    v
}

/// Say what a device is, and what set it belongs to.
pub fn cmd_identify(target: &str, at: Option<u64>) -> Res<()> {
    let (disk, base, name) = open_target(target, at)?;
    let size = disk.len()?;
    eprintln!("[*] {name} ({})", human(size));

    // Probe the device itself, then each partition on it. A NAS disk carries
    // its RAID metadata on the partition, not the disk.
    let mut spots = vec![(base, String::from("whole device"), 0u64)];
    let mut have_table = false;
    if at.is_none()
        && disk_arg(target).is_some()
        && let Ok(l) = read_layout(&disk)
    {
        have_table = true;
        for p in &l.parts {
            spots.push((
                p.start_lba * l.sector,
                format!("partition {} {:?}", p.number, p.name),
                p.sectors() * l.sector,
            ));
        }
    }
    if !have_table && at.is_none() && disk_arg(target).is_some() {
        eprintln!("[*] no partition table here -- neither GPT nor MBR");
    }

    let mut found = 0;
    for (off, what, len) in spots {
        // Probes that look at both ends of a thing need its length, not the
        // rest of the disk.
        let span = if len > 0 {
            len
        } else {
            size.saturating_sub(off)
        };
        let reports = identify::identify(&disk, off, span).unwrap_or_default();
        let fs = Fs::open(&disk, off).ok();

        // Whole-device silence is normal on a partitioned disk; a partition
        // that nothing recognises is worth saying out loud.
        let is_device = off == base && len == 0;
        if reports.is_empty() && fs.is_none() && is_device {
            continue;
        }
        eprintln!(
            "\r
{what} at {}{}",
            human(off),
            if len > 0 {
                format!(", {}", human(len))
            } else {
                String::new()
            }
        );
        // Count per spot, not overall: whether this partition was recognised
        // has nothing to do with whether an earlier one was.
        let mut here = 0;
        for r in reports {
            eprintln!("  {}", r.kind);
            for l in r.lines {
                eprintln!("      {l}");
            }
            here += 1;
        }
        if let Some(f) = fs {
            eprintln!("  {}", f.describe());
            eprintln!("      readable: bulkhead ls {target} --at {off}");
            here += 1;
        }
        if here == 0 {
            eprintln!("  nothing recognised here");
        }
        found += here;
    }
    if found == 0 {
        eprintln!(
            "\r
[*] nothing recognised. If the partition table is gone, try:"
        );
        eprintln!("    bulkhead scan {target}");
    }
    Ok(())
}

/// A filesystem whose device handle has been given away, so it can live for
/// as long as the process does.
type FsHandle = Fs<'static>;

/// Mount a filesystem Windows cannot read, as a drive.
pub fn cmd_mount_fs(target: &str, at: Option<u64>, mount_point: &str, debug: bool) -> Res<()> {
    let (disk, base, name) = open_target(target, at)?;
    // Leak the device handle so the filesystem outlives this frame. The mount
    // serves until the process is interrupted, so one handle is a small price
    // for not building a self-referential struct.
    let disk: &'static Raw = Box::leak(Box::new(disk));
    let fs = Fs::open(disk, base)?;
    eprintln!("[*] {name} at {}: {}", human(base), fs.describe());
    let (label, total) = (fs.label(), fs.total());
    let label = if label.is_empty() {
        "bulkhead".into()
    } else {
        label
    };
    winfsp::mount(fs, mount_point, &label, total, debug)
}

/// List a directory on a filesystem Windows cannot read.
/// Files and bytes under a directory, all the way down.
///
/// `ls` showed a directory as a bare name, so a directory holding a 40 GB VM
/// image looked exactly like an empty one -- and a volume got erased on the
/// strength of that listing. The walk is the same one `cmd_cp` does, so what
/// `ls` reports and what `cp` would carry off cannot drift apart.
///
/// Iterative and inode-guarded: a directory loop on a damaged volume would
/// otherwise hang, and unlike `cp` there is no growing output to notice it by.
fn tally(fs: &Fs, ino: u64) -> (u64, u64) {
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut seen = std::collections::HashSet::from([ino]);
    let mut queue = vec![ino];
    // ponytail: walks the whole tree on every ls. Fine for the volumes this
    // reads; add a depth cap if someone points it at millions of inodes.
    while let Some(dir) = queue.pop() {
        let Ok(entries) = fs.read_dir(dir) else {
            continue;
        };
        for e in entries {
            if e.is_dir {
                if seen.insert(e.inode) {
                    queue.push(e.inode);
                }
            } else {
                files += 1;
                bytes += fs.size_of(e.inode).unwrap_or(0);
            }
        }
    }
    (files, bytes)
}

pub fn cmd_ls(target: &str, at: Option<u64>, path: &str) -> Res<()> {
    let (disk, base, name) = open_target(target, at)?;
    let fs = Fs::open(&disk, base)?;
    eprintln!("[*] {name} at {}: {}", human(base), fs.describe());

    let (ino, is_dir) = fs.resolve(path)?;
    if !is_dir {
        eprintln!("  {} ({})", path, human(fs.size_of(ino)?));
        return Ok(());
    }
    let mut entries = fs.read_dir(ino)?;
    entries.sort_by_key(|a| (!a.is_dir, a.name.to_lowercase()));
    let (mut deep_files, mut deep_bytes) = (0u64, 0u64);
    for e in &entries {
        if e.is_dir {
            let (files, bytes) = tally(&fs, e.inode);
            deep_files += files;
            deep_bytes += bytes;
            let note = match files {
                0 => "empty".into(),
                1 => "1 file".into(),
                n => format!("{n} files"),
            };
            eprintln!("  {:>12}  {}/  {note}", human(bytes), e.name);
        } else {
            let size = fs.size_of(e.inode).unwrap_or(0);
            deep_files += 1;
            deep_bytes += size;
            eprintln!("  {:>12}  {}", human(size), e.name);
        }
    }
    // Never just "N entries": that reads as a total, and here it was believed
    // as one.
    eprintln!(
        "[*] {} entries here, {deep_files} files in all, {}",
        entries.len(),
        human(deep_bytes)
    );
    Ok(())
}

/// Copy a file or directory tree off a filesystem Windows cannot read.
pub fn cmd_cp(target: &str, at: Option<u64>, path: &str, out_dir: &str) -> Res<()> {
    let (disk, base, name) = open_target(target, at)?;
    let fs = Fs::open(&disk, base)?;
    eprintln!(
        "[*] {name}: {} -- copying {path:?} to {out_dir}",
        fs.describe()
    );
    std::fs::create_dir_all(out_dir)?;

    let (ino, is_dir) = fs.resolve(path)?;
    let leaf = path
        .rsplit(['/', '\\'])
        .find(|p| !p.is_empty())
        .unwrap_or("root");
    let (mut files, mut bytes) = (0u64, 0u64);

    if !is_dir {
        let data = fs.read_file(ino)?;
        std::fs::write(std::path::Path::new(out_dir).join(safe_name(leaf)), &data)?;
        eprintln!("[+] 1 file, {}", human(data.len() as u64));
        return Ok(());
    }

    // Iterative rather than recursive: a directory loop on a damaged volume
    // would take the stack with it.
    let mut queue = vec![(ino, std::path::PathBuf::from(out_dir).join(safe_name(leaf)))];
    while let Some((dir_ino, dest)) = queue.pop() {
        std::fs::create_dir_all(&dest)?;
        for e in fs.read_dir(dir_ino)? {
            let child = dest.join(safe_name(&e.name));
            if e.is_dir {
                queue.push((e.inode, child));
            } else {
                match fs.read_file(e.inode) {
                    Ok(data) => {
                        std::fs::write(&child, &data)?;
                        files += 1;
                        bytes += data.len() as u64;
                    }
                    Err(err) => eprintln!("[!] {}: {err}", e.name),
                }
            }
        }
    }
    eprintln!("[+] {files} file(s), {}", human(bytes));
    Ok(())
}

/// Names come off another operating system's filesystem; keep them from
/// naming anything but a file in the output directory.
fn safe_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if r#"\/:*?"<>|"#.contains(c) || (c as u32) < 32 {
                '_'
            } else {
                c
            }
        })
        .collect();
    match cleaned.trim().trim_matches('.') {
        "" => "_".into(),
        t => t.to_string(),
    }
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
pub fn parse_size(s: &str) -> Option<u64> {
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
        Ok(Table {
            header,
            array,
            sector,
        })
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

/// The partitions on a disk, however that disk happens to record them.
///
/// GPT and MBR are different tables answering the same question, so both come
/// back as one list in disk order and the commands above are spared the
/// difference. The usable range comes with it because GPT states it outright
/// and MBR only implies it.
struct Layout {
    parts: Vec<gpt::Entry>,
    first_usable: u64,
    last_usable: u64,
    sector: u64,
    kind: &'static str,
}

fn read_layout(disk: &Raw) -> Res<Layout> {
    let sector = disk.sector_size()? as u64;
    if let Ok(t) = Table::read(disk) {
        return Ok(Layout {
            parts: gpt::entries(&t.header, &t.array),
            first_usable: gpt::first_usable(&t.header),
            last_usable: gpt::last_usable(&t.header),
            sector: t.sector,
            kind: "GPT",
        });
    }

    let mut lba0 = vec![0u8; sector as usize];
    disk.seek(0)?;
    disk.read(&mut lba0)?;
    if !mbr::is_mbr(&lba0) {
        return Err("no partition table here -- no GPT, and no boot signature for MBR".into());
    }
    // A protective MBR with no readable GPT behind it is a damaged GPT disk,
    // not an MBR one. Saying "MBR disk, no partitions" would send someone
    // looking in the wrong place for data that is still there.
    if mbr::is_protective(&lba0) {
        return Err(
            "this disk has a GPT protective MBR but its GPT would not read -- \
                    the table is damaged rather than missing. Try: bulkhead scan"
                .into(),
        );
    }

    Ok(Layout {
        parts: mbr::entries(disk, &lba0, sector),
        // MBR declares no usable range. Everything after the boot sector is
        // fair game, and the disk ends where the disk ends.
        first_usable: 1,
        last_usable: disk.len()? / sector - 1,
        sector,
        kind: "MBR",
    })
}

pub fn cmd_part_list(disk_no: u32) -> Res<()> {
    let disk = Raw::open(&format!(r"\\.\PhysicalDrive{disk_no}"), false).ctx("open disk")?;
    let size = disk.len()?;
    let l = read_layout(&disk)?;

    eprintln!(
        "disk {disk_no}: {} ({}-byte sectors, {})",
        human(size),
        l.sector,
        l.kind
    );
    let mut pos = l.first_usable;
    for p in &l.parts {
        if p.start_lba > pos {
            eprintln!(
                "     {:>12}  {:>10}  (free)",
                human(pos * l.sector),
                human((p.start_lba - pos) * l.sector)
            );
        }
        eprintln!(
            "  {}  {:>12}  {:>10}  {}",
            p.number,
            human(p.start_lba * l.sector),
            human(p.sectors() * l.sector),
            p.name
        );
        pos = p.end_lba + 1;
    }
    if l.last_usable > pos {
        eprintln!(
            "     {:>12}  {:>10}  (free)",
            human(pos * l.sector),
            human((l.last_usable - pos + 1) * l.sector)
        );
    }
    Ok(())
}

/// Move a partition's data and repoint its table entry.
///
/// Windows cannot do this at any price, and it is what makes "extend into the
/// free space to the left" possible: slide the partition down, then extend it
/// with the native tools.
pub fn cmd_part_move(disk_no: u32, number: usize, to: u64, yes: bool) -> Res<()> {
    let sys = ps("(Get-Partition -DriveLetter C -ErrorAction SilentlyContinue).DiskNumber")?;
    if sys.trim() == disk_no.to_string() {
        return Err(format!(
            "disk {disk_no} holds the running C:. Move partitions from the recovery media."
        )
        .into());
    }

    let disk = Raw::open(&format!(r"\\.\PhysicalDrive{disk_no}"), true).ctx("open disk")?;
    let mut t = Table::read(&disk)?;
    let parts = gpt::entries(&t.header, &t.array);
    let me = parts
        .iter()
        .find(|p| p.number == number)
        .ok_or_else(|| format!("disk {disk_no} has no partition {number}"))?;

    if !to.is_multiple_of(t.sector) {
        return Err(format!("offset must be a multiple of the {}-byte sector", t.sector).into());
    }
    let new_start = to / t.sector;
    let len = me.sectors();
    let new_end = new_start + len - 1;

    if new_start < gpt::first_usable(&t.header) || new_end > gpt::last_usable(&t.header) {
        return Err(format!(
            "{} .. {} is outside the usable area ({} .. {})",
            human(new_start * t.sector),
            human((new_end + 1) * t.sector),
            human(gpt::first_usable(&t.header) * t.sector),
            human((gpt::last_usable(&t.header) + 1) * t.sector)
        )
        .into());
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

    eprintln!(
        "\n[!] Moving disk {disk_no} partition {number} ({}) from {} to {}",
        human(len * t.sector),
        human(me.start_lba * t.sector),
        human(to)
    );
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

    eprintln!(
        "[*] moving {} {}",
        human(len),
        if backwards { "(backwards)" } else { "" }
    );
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

pub fn cmd_mount(path: &str, rw: bool) -> Res<()> {
    let vhd = Vhd::open(path, rw)?;
    vhd.attach(!rw, true, true)?;
    eprintln!(
        "[+] attached {} ({})",
        vhd.physical_path()?,
        if rw { "read-write" } else { "read-only" }
    );
    eprintln!("    it appears in Explorer; detach with:  bulkhead unmount {path}");
    Ok(())
}

pub fn cmd_unmount(path: &str) -> Res<()> {
    // matches however it was attached
    let vhd = Vhd::open(path, true).or_else(|_| Vhd::open(path, false))?;
    vhd.detach()?;
    eprintln!("[+] detached {path}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        (
            disk[to..to + len].to_vec(),
            original[from..from + len].to_vec(),
        )
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
        let d = |v: u64| (v + 8 * MB).div_ceil(MB) * MB;
        assert!(d(100 * MB + 1) > 100 * MB && d(100 * MB + 1) % MB == 0);
    }
}
