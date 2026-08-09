//! What is this disk, and what was it part of?
//!
//! The question you actually have when someone hands you an unlabelled drive
//! out of a dead NAS. Reading a filesystem is a lot of work; recognising one,
//! and recognising the RAID or volume-manager layer *underneath* it, is very
//! little -- and on a NAS disk the layer underneath is the thing standing
//! between you and any filesystem at all.
//!
//! Every probe here is read-only and reports what a header says about itself.
use crate::util::{human, Res};
use crate::Raw;

pub struct Report {
    pub kind: &'static str,
    pub lines: Vec<String>,
}

fn le16(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u64
}
fn le32(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn be32(b: &[u8], o: usize) -> u64 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

fn text(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

/// Only report a name if it looks like one; a wrong label is worse than none.
fn printable(s: String) -> Option<String> {
    let ok = !s.is_empty() && s.chars().all(|c| c == ' ' || (!c.is_control() && c.is_ascii()));
    ok.then_some(s)
}

pub fn uuid_str(b: &[u8]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    if h.len() < 32 {
        return h;
    }
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// What a device's slot number means in an md array.
pub fn md_role(role: u16) -> String {
    match role {
        0xFFFF => "spare".into(),
        0xFFFE => "faulty".into(),
        n => format!("member {n}"),
    }
}

/// `base` is where the thing being probed starts: a partition's offset, or
/// 0 for a whole device. Every structure below is placed relative to it.
fn read_at(disk: &Raw, base: u64, off: u64, len: usize) -> Option<Vec<u8>> {
    let off = base + off;
    const SECTOR: u64 = 512;
    let start = off / SECTOR * SECTOR;
    let skip = (off - start) as usize;
    let total = (skip + len).div_ceil(SECTOR as usize) * SECTOR as usize;
    let mut b = vec![0u8; total];
    disk.seek(start).ok()?;
    let got = disk.read(&mut b).ok()?;
    if got < skip + len {
        return None;
    }
    b.drain(..skip);
    b.truncate(len);
    Some(b)
}

// --- Linux software RAID ----------------------------------------------------

const MD_MAGIC: u64 = 0xa92b_4efc;

fn md_level(l: u64) -> String {
    match l {
        0 => "RAID0 (striped, no redundancy)".into(),
        1 => "RAID1 (mirror)".into(),
        4 => "RAID4".into(),
        5 => "RAID5".into(),
        6 => "RAID6".into(),
        10 => "RAID10".into(),
        u64::MAX => "linear".into(),
        n => format!("level {n}"),
    }
}

/// mdadm metadata, which sits in one of three places depending on version:
/// 4 KiB in (1.2, the default), at the very start (1.1), or near the end (1.0).
fn md_raid(disk: &Raw, base: u64, size: u64) -> Option<Report> {
    let candidates = [
        4096u64,
        0,
        // v1.0: 8 KiB from the end, rounded down to a 4 KiB boundary.
        size.checked_sub(8 * 1024)? / 4096 * 4096,
    ];
    let sb = candidates.iter().find_map(|&off| {
        let b = read_at(disk, base, off, 4096)?;
        (le32(&b, 0) == MD_MAGIC && le32(&b, 4) == 1).then_some(b)
    })?;

    let raid_disks = le32(&sb, 0x5C);
    let dev_number = le32(&sb, 0xA0);
    // dev_roles is an array indexed by device number, at the end of the header.
    let role = sb
        .get(0x100 + dev_number as usize * 2..0x100 + dev_number as usize * 2 + 2)
        .map(|r| le16(r, 0) as u16);

    let mut lines = vec![
        format!("array {:?}", text(&sb[0x20..0x40])),
        format!("array UUID {}", uuid_str(&sb[0x10..0x20])),
        format!("{}, {raid_disks} devices", md_level(le32(&sb, 0x48))),
        format!("this disk is device {dev_number}{}",
                role.map(|r| format!(", {}", md_role(r))).unwrap_or_default()),
        format!("chunk {}, data starts {} in",
                human(le32(&sb, 0x58) * 512), human(le64(&sb, 0x80) * 512)),
        format!("events {} -- members with different counts are out of sync",
                le64(&sb, 0xC8)),
    ];
    lines.push("assemble with mdadm on Linux before the filesystem is reachable".into());
    Some(Report { kind: "Linux MD RAID member", lines })
}

// --- LVM2 -------------------------------------------------------------------

fn lvm2(disk: &Raw, base: u64) -> Option<Report> {
    // The label lives in one of the first four sectors and says which.
    let head = read_at(disk, base, 0, 2048)?;
    let sector = (0..4).find(|&s| &head[s * 512..s * 512 + 8] == b"LABELONE")?;
    let lbl = &head[sector * 512..];
    if &lbl[0x18..0x20] != b"LVM2 001" {
        return None;
    }
    let pvh = le32(lbl, 0x14) as usize;
    let mut lines = vec![format!("PV UUID {}", text(&lbl[pvh..pvh + 32]))];
    lines.push(format!("device size {}", human(le64(lbl, pvh + 32))));

    // The volume group's name is in the text metadata, which begins with it.
    let mda = read_at(disk, base, 4096, 4096).and_then(|m| {
        let start = m.iter().position(|&c| c.is_ascii_alphanumeric() || c == b'_')?;
        let s = String::from_utf8_lossy(&m[start..]);
        let name = s.split_whitespace().next()?.to_string();
        (!name.is_empty() && name.len() < 128).then_some(name)
    });
    if let Some(vg) = mda {
        lines.push(format!("volume group {vg:?}"));
    }
    lines.push("activate with vgchange on Linux to reach the volumes".into());
    Some(Report { kind: "LVM2 physical volume", lines })
}

// --- ZFS --------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub enum Nv {
    U64(u64),
    Str(String),
    Other,
}

/// Enough XDR to read the top level of a ZFS vdev label.
///
/// Every pair records its own encoded size, so anything unrecognised -- nested
/// lists, arrays -- can be stepped over without understanding it. That is what
/// makes reading the interesting scalars cheap.
pub fn nvlist_pairs(b: &[u8]) -> Vec<(String, Nv)> {
    let mut out = Vec::new();
    // 4 bytes of encoding header, then version and flags.
    let mut off = 4 + 8;
    while off + 8 <= b.len() {
        let encoded = be32(b, off) as usize;
        if encoded == 0 {
            break; // end of list
        }
        let name_len = match b.get(off + 8..off + 12) {
            Some(x) => be32(x, 0) as usize,
            None => break,
        };
        let name_at = off + 12;
        let padded = name_len.div_ceil(4) * 4;
        let Some(name) = b.get(name_at..name_at + name_len) else { break };
        let name = String::from_utf8_lossy(name).into_owned();

        let t_at = name_at + padded;
        let Some(ty) = b.get(t_at..t_at + 4).map(|x| be32(x, 0)) else { break };
        let val_at = t_at + 8; // past type and element count
        let v = match ty {
            8 => b.get(val_at..val_at + 8).map(|x| Nv::U64(be64(x, 0))).unwrap_or(Nv::Other),
            9 => b
                .get(val_at..val_at + 4)
                .and_then(|x| {
                    let n = be32(x, 0) as usize;
                    b.get(val_at + 4..val_at + 4 + n)
                })
                .map(|x| Nv::Str(String::from_utf8_lossy(x).into_owned()))
                .unwrap_or(Nv::Other),
            _ => Nv::Other,
        };
        out.push((name, v));
        if off + encoded <= off {
            break;
        }
        off += encoded;
    }
    out
}

fn zfs(disk: &Raw, base: u64, size: u64) -> Option<Report> {
    // Four identical labels: two at the front, two at the back. Any will do,
    // and trying more than one survives a damaged front of disk.
    let spots = [16 * 1024u64, 256 * 1024 + 16 * 1024,
                 size.checked_sub(512 * 1024)? + 16 * 1024,
                 size.checked_sub(256 * 1024)? + 16 * 1024];
    for off in spots {
        let Some(b) = read_at(disk, base, off, 112 * 1024) else { continue };
        let pairs = nvlist_pairs(&b);
        let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v);
        let Some(Nv::Str(name)) = get("name") else { continue };

        let mut lines = vec![format!("pool {name:?}")];
        if let Some(Nv::U64(g)) = get("pool_guid") {
            lines.push(format!("pool GUID {g:#x}"));
        }
        if let Some(Nv::U64(g)) = get("guid") {
            lines.push(format!("this device GUID {g:#x}"));
        }
        if let Some(Nv::U64(s)) = get("state") {
            lines.push(format!("state {}", match s {
                0 => "active".into(),
                1 => "exported".into(),
                2 => "destroyed".into(),
                n => format!("{n}"),
            }));
        }
        if let Some(Nv::U64(t)) = get("txg") {
            lines.push(format!("txg {t} -- members with different txgs are out of sync"));
        }
        if let Some(Nv::Str(h)) = get("hostname") {
            lines.push(format!("last used by host {h:?}"));
        }
        lines.push("import with zpool on a system that speaks ZFS; bulkhead does not read it".into());
        return Some(Report { kind: "ZFS pool member", lines });
    }
    None
}

// --- btrfs ------------------------------------------------------------------

fn btrfs(disk: &Raw, base: u64) -> Option<Report> {
    let sb = read_at(disk, base, 0x10000, 4096)?;
    if &sb[0x40..0x48] != b"_BHRfS_M" {
        return None;
    }
    let devices = le64(&sb, 0x88);
    let mut lines = vec![
        format!("filesystem UUID {}", uuid_str(&sb[0x20..0x30])),
        format!("{} of {} device(s) in this filesystem", 1, devices),
        format!("this device is id {}", le64(&sb, 0xC9)),
        format!("{} used of {}", human(le64(&sb, 0x78)), human(le64(&sb, 0x70))),
    ];
    if let Some(l) = printable(text(&sb[0x12B..0x22B])) {
        lines.insert(0, format!("label {l:?}"));
    }
    if devices > 1 {
        lines.push("multi-device: every member is needed before this mounts".into());
    }
    Some(Report { kind: "btrfs", lines })
}

// --- bcachefs ---------------------------------------------------------------

const BCACHEFS_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x4e, 0x1a, 0x45, 0xca,
    0x82, 0x65, 0xf5, 0x7f, 0x48, 0xba, 0x6d, 0x81,
];

fn bcachefs(disk: &Raw, base: u64) -> Option<Report> {
    let sb = read_at(disk, base, 4096, 4096)?;
    if sb[0x18..0x28] != BCACHEFS_MAGIC {
        return None;
    }
    let mut lines = vec![
        format!("filesystem UUID {}", uuid_str(&sb[0x38..0x48])),
        format!("device {} of {}", sb[0x7A], sb[0x7B]),
        format!("format version {}", le16(&sb, 0x10)),
    ];
    if let Some(l) = printable(text(&sb[0x48..0x68])) {
        lines.insert(0, format!("label {l:?}"));
    }
    Some(Report { kind: "bcachefs", lines })
}

// --- SquashFS ---------------------------------------------------------------

fn squashfs(disk: &Raw, base: u64) -> Option<Report> {
    let sb = read_at(disk, base, 0, 512)?;
    if &sb[0..4] != b"hsqs" {
        return None;
    }
    let comp = match le16(&sb, 0x14) {
        1 => "gzip",
        2 => "lzma",
        3 => "lzo",
        4 => "xz",
        5 => "lz4",
        6 => "zstd",
        _ => "unknown",
    };
    Some(Report {
        kind: "SquashFS image",
        lines: vec![
            format!("version {}.{}", le16(&sb, 0x1C), le16(&sb, 0x1E)),
            format!("{} inodes, {} used, {} blocks, {comp} compressed",
                    le32(&sb, 0x04), human(le64(&sb, 0x28)), human(le32(&sb, 0x0C))),
            "contents are compressed; bulkhead identifies these but does not unpack them".into(),
        ],
    })
}

// --- UFS2 -------------------------------------------------------------------

fn ufs2(disk: &Raw, base: u64) -> Option<Report> {
    // The superblock is 64 KiB in, and its magic sits well inside it.
    let sb = read_at(disk, base, 65536, 4096)?;
    if le32(&sb, 0x55C) != 0x1954_0119 {
        return None;
    }
    let mut lines = vec![format!("block size {}", human(le32(&sb, 0x30)))];
    if let Some(m) = printable(text(&sb[0x2D8..0x2D8 + 200])) {
        lines.push(format!("last mounted on {m:?}"));
    }
    if let Some(v) = printable(text(&sb[0x4AC..0x4CC])) {
        lines.push(format!("volume name {v:?}"));
    }
    lines.push("FreeBSD/pfSense/TrueNAS; reading is not implemented yet".into());
    Some(Report { kind: "UFS2", lines })
}

// --- VMFS -------------------------------------------------------------------

fn vmfs(disk: &Raw, base: u64) -> Option<Report> {
    // Layout is reverse-engineered, so report only what is unambiguous and
    // drop anything that does not look like text.
    let fs = read_at(disk, base, 0x0020_0000, 512)?;
    if le32(&fs, 0) != 0x2fab_f15e {
        return None;
    }
    let mut lines = vec![format!("version {}", fs[8])];
    if let Some(l) = printable(text(&fs[0x20..0xA0])) {
        lines.insert(0, format!("label {l:?}"));
    }
    lines.push("VMware datastore; reading is not implemented".into());
    Some(Report { kind: "VMFS", lines })
}

// --- what Windows already handles -------------------------------------------

/// Name NTFS, exFAT and FAT where they are found.
///
/// bulkhead does not read these -- Windows does it better -- but saying so is
/// the difference between "here is your ESP" and a bare "nothing recognised"
/// on an entirely healthy disk.
fn windows_fs(disk: &Raw, base: u64) -> Option<Report> {
    let bs = read_at(disk, base, 0, 512)?;
    if bs[510..512] != [0x55, 0xAA] {
        return None;
    }
    let kind = if &bs[3..11] == b"NTFS    " {
        "NTFS"
    } else if &bs[3..11] == b"EXFAT   " {
        "exFAT"
    } else if &bs[0x52..0x57] == b"FAT32" {
        "FAT32"
    } else if matches!(&bs[0x36..0x3B], b"FAT16" | b"FAT12" | b"FAT  ") {
        "FAT"
    } else {
        return None;
    };
    Some(Report {
        kind: "filesystem",
        lines: vec![format!("{kind} -- Windows reads this natively; open it in Explorer")],
    })
}

/// Everything that recognises itself on this device.
pub fn identify(disk: &Raw, base: u64, size: u64) -> Res<Vec<Report>> {
    let mut out = Vec::new();
    for r in [
        md_raid(disk, base, size),
        lvm2(disk, base),
        zfs(disk, base, size),
        btrfs(disk, base),
        bcachefs(disk, base),
        squashfs(disk, base),
        ufs2(disk, base),
        vmfs(disk, base),
        windows_fs(disk, base),
    ]
    .into_iter()
    .flatten()
    {
        out.push(r);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_name_the_odd_ones() {
        assert_eq!(md_role(0), "member 0");
        assert_eq!(md_role(3), "member 3");
        assert_eq!(md_role(0xFFFF), "spare");
        assert_eq!(md_role(0xFFFE), "faulty");
    }

    #[test]
    fn uuids_are_grouped_the_usual_way() {
        let b: Vec<u8> = (0..16).collect();
        assert_eq!(uuid_str(&b), "00010203-0405-0607-0809-0a0b0c0d0e0f");
    }

    fn nvpair(name: &str, ty: u32, value: &[u8]) -> Vec<u8> {
        let name_pad = name.len().div_ceil(4) * 4;
        let size = 12 + name_pad + 8 + value.len();
        let mut v = (size as u32).to_be_bytes().to_vec();
        v.extend_from_slice(&0u32.to_be_bytes()); // decoded size
        v.extend_from_slice(&(name.len() as u32).to_be_bytes());
        v.extend_from_slice(name.as_bytes());
        v.extend(vec![0u8; name_pad - name.len()]);
        v.extend_from_slice(&ty.to_be_bytes());
        v.extend_from_slice(&1u32.to_be_bytes()); // element count
        v.extend_from_slice(value);
        v
    }

    #[test]
    fn nvlist_reads_strings_and_numbers() {
        let mut b = vec![0u8; 12]; // encoding header, version, flags
        let mut name = 4u32.to_be_bytes().to_vec();
        name.extend_from_slice(b"tank");
        b.extend(nvpair("name", 9, &name));
        b.extend(nvpair("pool_guid", 8, &0x1234_5678_9abc_def0u64.to_be_bytes()));
        b.extend_from_slice(&0u32.to_be_bytes()); // terminator

        let p = nvlist_pairs(&b);
        assert_eq!(p[0], ("name".into(), Nv::Str("tank".into())));
        assert_eq!(p[1], ("pool_guid".into(), Nv::U64(0x1234_5678_9abc_def0)));
    }

    #[test]
    fn nvlist_steps_over_what_it_does_not_understand() {
        let mut b = vec![0u8; 12];
        // a nested list in the middle must not stop the walk
        b.extend(nvpair("vdev_tree", 19, &[0u8; 32]));
        b.extend(nvpair("txg", 8, &99u64.to_be_bytes()));
        b.extend_from_slice(&0u32.to_be_bytes());

        let p = nvlist_pairs(&b);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].1, Nv::Other);
        assert_eq!(p[1], ("txg".into(), Nv::U64(99)));
    }

    #[test]
    fn nvlist_stops_cleanly_on_rubbish() {
        assert!(nvlist_pairs(&[]).is_empty());
        assert!(nvlist_pairs(&[0u8; 12]).is_empty());
        // a truncated pair must not read past the end
        let mut b = vec![0u8; 12];
        b.extend_from_slice(&999u32.to_be_bytes());
        assert!(nvlist_pairs(&b).is_empty());
    }

    #[test]
    fn labels_that_are_not_text_are_dropped() {
        assert_eq!(printable("backups".into()), Some("backups".into()));
        assert_eq!(printable(String::new()), None);
        assert_eq!(printable("\u{1}\u{2}\u{3}".into()), None);
    }
}
