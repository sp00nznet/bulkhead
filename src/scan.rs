//! Find filesystems on a disk whose partition table is gone.
//!
//! A partition table is a few kilobytes of pointers. Losing it does not touch
//! the filesystems themselves -- they are still sitting there, each one opening
//! with a header that says what it is and, crucially, how big it is. Scanning
//! for those headers reconstructs the table.
//!
//! Ported from partrevive, which does this on Linux. Every detector re-reads
//! the device to confirm the magic and returns the volume's *own* recorded
//! size, so nothing gets truncated by a guess.
use crate::Raw;
use crate::util::{Res, human};

const SECTOR: u64 = 512;
/// Anything smaller is noise, not a partition worth restoring.
const MIN_SECTORS: u64 = 2048;
const CHUNK: usize = 4 << 20;
/// How far past a candidate start a detector's magic can sit. btrfs keeps its
/// superblock 64 KiB in, the furthest of the lot. Reading this much beyond the
/// chunk means a candidate near the end can still be checked.
///
/// A multiple of the sector size, like every length here: reads from a raw
/// disk handle must be whole sectors.
const LOOKAHEAD: usize = 128 << 10;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub start_lba: u64,
    pub sectors: u64,
    pub fstype: &'static str,
    pub label: String,
    /// GPT partition type for the rebuilt table.
    pub gpt_type: &'static str,
    /// Detected and reported, but never sized or put in a proposed table --
    /// you cannot size a LUKS or LVM container from its header alone.
    pub report_only: bool,
    pub note: &'static str,
    /// Higher wins when two candidates claim the same ground. Corroborating
    /// structures a ghost cannot fake raise it -- see the NTFS detector.
    pub confidence: u8,
}

impl Candidate {
    fn new(start: u64, sectors: u64, fstype: &'static str, gpt_type: &'static str) -> Candidate {
        Candidate {
            start_lba: start / SECTOR,
            sectors,
            fstype,
            label: String::new(),
            gpt_type,
            report_only: false,
            note: "",
            confidence: 1,
        }
    }
    fn with_label(mut self, l: String) -> Candidate {
        self.label = l;
        self
    }
    pub fn end_lba(&self) -> u64 {
        self.start_lba
            .saturating_add(self.sectors)
            .saturating_sub(1)
    }
    pub fn bytes(&self) -> u64 {
        self.sectors.saturating_mul(SECTOR)
    }
}

const BASIC: &str = "{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}";
const LINUX: &str = "{0fc63daf-8483-4772-8e79-3d69d8477de4}";
const SWAP: &str = "{0657fd6d-a4ab-43c4-84e5-0933c84b4f4f}";
const LUKS: &str = "{ca7d7ccb-63ed-4c53-861c-1742536059cc}";
const LVM: &str = "{e6d6d379-f507-44c2-a23c-238f2a3df928}";

fn le(b: &[u8]) -> u64 {
    let mut v = 0u64;
    for (i, &x) in b.iter().enumerate().take(8) {
        v |= (x as u64) << (i * 8);
    }
    v
}

fn be32(b: &[u8]) -> u64 {
    u32::from_be_bytes(b[..4].try_into().unwrap()) as u64
}

fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

/// A shift whose amount came off the disk.
///
/// Every value feeding one of these is arbitrary data in practice: a scan
/// walks the whole disk, and some of it will match a magic by chance. Garbage
/// has to reject the candidate, not panic the process. The ceiling is a
/// generous bound on any real block or cluster size.
fn shl(base: u64, amount: u64) -> Option<u64> {
    if amount >= 32 {
        return None;
    }
    let v = base << amount;
    (SECTOR..=(1 << 26)).contains(&v).then_some(v)
}

/// Convert a count of `unit`-sized blocks into sectors, rejecting anything
/// that does not fit a plausible disk.
fn sectors_of(count: u64, unit: u64) -> Option<u64> {
    let per = (unit / SECTOR).max(1);
    count.checked_mul(per).filter(|&s| s > 0 && s < (1 << 40))
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn at(disk: &Raw, off: u64, len: usize) -> Option<Vec<u8>> {
    let mut v = vec![0u8; len];
    disk.seek(off).ok()?;
    if disk.read(&mut v).ok()? != len {
        return None;
    }
    Some(v)
}

/// FAT, NTFS and exFAT boot sectors all begin with a jump instruction. Cheap,
/// and it rejects most stray occurrences of the magic in file data.
fn has_jump(bs: &[u8]) -> bool {
    bs[0] == 0xEB || bs[0] == 0xE9
}

fn ntfs(disk: &Raw, start: u64) -> Option<Candidate> {
    let bs = at(disk, start, 512)?;
    if &bs[3..11] != b"NTFS    " || bs[510..512] != [0x55, 0xAA] || !has_jump(&bs) {
        return None;
    }
    let total = le(&bs[0x28..0x30]);
    if total == 0 || total > (1 << 40) {
        return None;
    }

    // Follow the boot sector to the $MFT and check it is really there.
    //
    // This is what separates a live filesystem from a ghost. Repartitioning or
    // moving a volume leaves the old boot sector intact wherever it used to
    // start, and it still reports a plausible size -- but the $MFT it points at
    // has been overwritten by whatever occupies that ground now.
    let bps = le(&bs[0x0B..0x0D]);
    let spc = bs[0x0D] as i8;
    if !(256..=65536).contains(&bps) {
        return None;
    }
    let cluster = if spc > 0 {
        bps.checked_mul(spc as u64).filter(|&c| c <= (1 << 20))?
    } else {
        shl(1, -(spc as i32) as u64)?
    };
    let mft_off = le(&bs[0x30..0x38]).checked_mul(cluster)?;
    if mft_off >= total.checked_mul(bps)? {
        return None;
    }
    let rec = at(disk, start + mft_off, 512)?;
    if &rec[..4] != b"FILE" {
        return None;
    }

    // NTFS records its size excluding the backup boot sector that follows it.
    let mut c = Candidate::new(start, total + 1, "ntfs", BASIC);

    // The backup boot sector is a copy of the first, parked on the volume's
    // last sector. It is the strongest evidence a volume really occupies the
    // ground it claims: a boot sector left behind by an earlier layout is
    // still readable, but the tail it points at now belongs to something else.
    //
    // Missing it is not disqualifying -- a genuinely truncated volume is still
    // worth recovering -- so it raises confidence rather than acting as a veto.
    if let Some(bak) = at(disk, start.checked_add(total.checked_mul(bps)?)?, 512)
        && &bak[3..11] == b"NTFS    "
        && bak[510..512] == [0x55, 0xAA]
    {
        c.confidence += 1;
    }
    Some(c)
}

fn exfat(disk: &Raw, start: u64) -> Option<Candidate> {
    let bs = at(disk, start, 512)?;
    if &bs[3..11] != b"EXFAT   " || bs[510..512] != [0x55, 0xAA] || !has_jump(&bs) {
        return None;
    }
    let len = le(&bs[0x48..0x50]);
    (len > 0 && len < (1 << 40)).then(|| Candidate::new(start, len, "exfat", BASIC))
}

fn fat(disk: &Raw, start: u64) -> Option<Candidate> {
    let bs = at(disk, start, 512)?;
    if bs[510..512] != [0x55, 0xAA] || !has_jump(&bs) {
        return None;
    }
    let is32 = &bs[0x52..0x57] == b"FAT32";
    let is16 = matches!(&bs[0x36..0x3B], b"FAT16" | b"FAT12" | b"FAT  ");
    if !is32 && !is16 {
        return None;
    }
    let tot16 = le(&bs[0x13..0x15]);
    let total = if tot16 != 0 {
        tot16
    } else {
        le(&bs[0x20..0x24])
    };
    (total > 0).then(|| Candidate::new(start, total, "fat", BASIC))
}

fn ext(disk: &Raw, start: u64) -> Option<Candidate> {
    let sb = at(disk, start + 1024, 1024)?;
    if sb[0x38..0x3A] != [0x53, 0xEF] {
        return None;
    }
    // s_block_group_nr != 0 means this is one of the backup superblocks kept
    // inside the filesystem, not the start of a partition.
    if le(&sb[0x5A..0x5C]) != 0 {
        return None;
    }
    let blocks = le(&sb[0x04..0x08]);
    let block_size = shl(1024, le(&sb[0x18..0x1C]))?;
    Some(
        Candidate::new(start, sectors_of(blocks, block_size)?, "ext4", LINUX)
            .with_label(cstr(&sb[0x78..0x88])),
    )
}

fn swap(disk: &Raw, start: u64) -> Option<Candidate> {
    let page = at(disk, start, 4096)?;
    if !matches!(&page[4086..4096], b"SWAPSPACE2" | b"SWAP-SPACE") {
        return None;
    }
    let last_page = le(&page[0x408..0x40C]);
    let sectors = sectors_of(last_page.checked_add(1)?, 4096)?;
    (last_page > 0).then(|| Candidate::new(start, sectors, "swap", SWAP))
}

fn btrfs(disk: &Raw, start: u64) -> Option<Candidate> {
    let sb = at(disk, start + 0x10000, 4096)?;
    if &sb[0x40..0x48] != b"_BHRfS_M" {
        return None;
    }
    // bytenr says which copy this is; btrfs mirrors its superblock at 64 MiB
    // and 256 GiB, and those are not partition starts.
    if le(&sb[0x30..0x38]) != 0x10000 {
        return None;
    }
    let total = le(&sb[0x70..0x78]);
    let sectorsize = le(&sb[0x90..0x94]);
    if total == 0 || !(512..=65536).contains(&sectorsize) || !sectorsize.is_power_of_two() {
        return None;
    }
    let sectors = (total / SECTOR).min(1 << 40);
    Some(Candidate::new(start, sectors, "btrfs", LINUX).with_label(cstr(&sb[0x12B..0x22B])))
}

fn xfs(disk: &Raw, start: u64) -> Option<Candidate> {
    let bs = at(disk, start, 512)?;
    if &bs[0..4] != b"XFSB" {
        return None;
    }
    // XFS is big-endian on disk, unlike everything else here.
    let blocksize = be32(&bs[4..8]);
    let dblocks = be64(&bs[8..16]);
    if dblocks == 0 || !(512..=65536).contains(&blocksize) || !blocksize.is_power_of_two() {
        return None;
    }
    Some(
        Candidate::new(start, sectors_of(dblocks, blocksize)?, "xfs", LINUX)
            .with_label(cstr(&bs[0x6C..0x78])),
    )
}

fn f2fs(disk: &Raw, start: u64) -> Option<Candidate> {
    let sb = at(disk, start + 0x400, 512)?;
    if sb[0..4] != [0x10, 0x20, 0xF5, 0xF2] {
        return None;
    }
    let blocksize = shl(1, le(&sb[0x10..0x14]))?;
    let blocks = le(&sb[0x24..0x2C]);
    Some(Candidate::new(
        start,
        sectors_of(blocks, blocksize)?,
        "f2fs",
        LINUX,
    ))
}

fn luks(disk: &Raw, start: u64) -> Option<Candidate> {
    // 512, not 8: reads from a raw disk handle must be a whole sector.
    let bs = at(disk, start, 512)?;
    if &bs[0..6] != b"LUKS\xba\xbe" {
        return None;
    }
    let mut c = Candidate::new(start, MIN_SECTORS, "LUKS", LUKS);
    c.report_only = true;
    c.note = "encrypted -- unlock it to recover, size is unknown from the header";
    Some(c)
}

fn lvm(disk: &Raw, start: u64) -> Option<Candidate> {
    let lh = at(disk, start, 512)?;
    if &lh[0..8] != b"LABELONE" || &lh[0x20..0x28] != b"LVM2 001" {
        return None;
    }
    // The label may sit in any of the PV's first four sectors, and records
    // which one, so the PV start can be worked back from it.
    let label_sector = le(&lh[8..16]);
    let pv_start = start.checked_sub(label_sector * SECTOR)?;
    let mut c = Candidate::new(pv_start, MIN_SECTORS, "LVM2", LVM);
    c.report_only = true;
    c.note = "LVM physical volume -- activate the volume group to recover";
    Some(c)
}

type Detector = fn(&Raw, u64) -> Option<Candidate>;

/// magic, its byte offset from the partition start, detector.
const SIGNATURES: &[(&[u8], u64, Detector)] = &[
    (b"NTFS    ", 3, ntfs),
    (b"EXFAT   ", 3, exfat),
    (b"FAT32   ", 0x52, fat),
    (b"FAT16   ", 0x36, fat),
    (b"FAT12   ", 0x36, fat),
    (&[0x53, 0xEF], 0x438, ext),
    (b"_BHRfS_M", 0x10040, btrfs),
    (b"XFSB", 0, xfs),
    (&[0x10, 0x20, 0xF5, 0xF2], 0x400, f2fs),
    (b"SWAPSPACE2", 4086, swap),
    (b"SWAP-SPACE", 4086, swap),
    (b"LUKS\xba\xbe", 0, luks),
    (b"LABELONE", 0, lvm),
];

/// Run every detector at one known offset.
///
/// `scan` sweeps a disk looking for these; `identify` already knows where a
/// partition starts and only wants to know what is on it. Same detectors
/// either way -- including the corroboration that separates a live filesystem
/// from a header left behind by a previous one.
pub fn probe(disk: &Raw, start: u64) -> Option<Candidate> {
    SIGNATURES
        .iter()
        .find_map(|(_, _, detect)| detect(disk, start))
}

pub fn scan(disk: &Raw, disk_size: u64) -> Res<Vec<Candidate>> {
    let mut found: Vec<Candidate> = Vec::new();
    let mut seen_starts: Vec<u64> = Vec::new();
    let mut buf = vec![0u8; CHUNK + LOOKAHEAD];
    let mut pos = 0u64;
    let mut last_pct = u64::MAX;

    while pos < disk_size {
        let want = ((disk_size - pos) as usize).min(buf.len());
        disk.seek(pos)?;
        let n = disk.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }

        // Walk sector boundaries, not bytes. A filesystem always begins on
        // one, so a magic anywhere else is file contents that happen to look
        // like a header -- and checking one position in 512 is the difference
        // between this finishing and not.
        let mut off = 0usize;
        while off < n.min(CHUNK) {
            let start = pos + off as u64;
            if !seen_starts.contains(&start) {
                for (magic, moff, detect) in SIGNATURES {
                    let a = off + *moff as usize;
                    if a + magic.len() > n || &buf[a..a + magic.len()] != *magic {
                        continue;
                    }
                    if let Some(c) = detect(disk, start) {
                        let end = c.end_lba().saturating_mul(SECTOR);
                        if c.sectors >= MIN_SECTORS && end < disk_size {
                            seen_starts.push(c.start_lba * SECTOR);
                            eprintln!(
                                "\r  found {} at {} ({})      ",
                                c.fstype,
                                human(c.start_lba * SECTOR),
                                human(c.bytes())
                            );
                            found.push(c);
                            break;
                        }
                    }
                }
            }
            off += SECTOR as usize;
        }

        pos += CHUNK as u64;
        let pct = (pos.min(disk_size)) * 100 / disk_size;
        if pct != last_pct {
            eprint!(
                "\r  {pct:3}%  {} / {}",
                human(pos.min(disk_size)),
                human(disk_size)
            );
            use std::io::Write;
            let _ = std::io::stderr().flush();
            last_pct = pct;
        }
    }
    // Same shape as the progress line it overwrites, or the tail of the longer
    // one is left behind.
    eprintln!(
        "\r  100%  {} / {}      ",
        human(disk_size),
        human(disk_size)
    );

    found.sort_by_key(|c| (c.start_lba, u64::MAX - c.sectors));
    Ok(dedup_contained(found))
}

/// Drop candidates that are backup superblocks rather than partitions.
///
/// ext, XFS, FAT and f2fs all keep spare copies of their superblock *inside*
/// the filesystem. A copy re-derives the same size, so it looks like a second
/// partition of identical length starting partway into the first. Same type
/// and same size, contained in an earlier one, is always a copy.
pub fn dedup_contained(cands: Vec<Candidate>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for c in cands {
        let copy = out.iter().any(|k| {
            k.fstype == c.fstype
                && k.sectors == c.sectors
                && c.start_lba > k.start_lba
                && c.start_lba <= k.end_lba()
        });
        if !copy {
            out.push(c);
        }
    }
    out
}

/// Pick a non-overlapping set, largest first where two claim the same ground.
///
/// Overlaps are real: a disk repartitioned once carries the ghost of the old
/// layout, and both sets of headers survive. Best-corroborated wins, then
/// largest -- size alone cannot separate a moved volume from the boot sector
/// it left behind, since both report the same length.
pub fn resolve(cands: &[Candidate]) -> (Vec<Candidate>, Vec<Candidate>) {
    let mut by_size: Vec<&Candidate> = cands.iter().filter(|c| !c.report_only).collect();
    by_size.sort_by_key(|c| (u8::MAX - c.confidence, u64::MAX - c.sectors));

    let (mut keep, mut drop): (Vec<Candidate>, Vec<Candidate>) = (Vec::new(), Vec::new());
    for c in by_size {
        let clash = keep
            .iter()
            .any(|k: &Candidate| c.start_lba <= k.end_lba() && k.start_lba <= c.end_lba());
        if clash {
            drop.push(c.clone());
        } else {
            keep.push(c.clone());
        }
    }
    keep.sort_by_key(|c| c.start_lba);
    drop.sort_by_key(|c| c.start_lba);
    (keep, drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(start: u64, sectors: u64, fstype: &'static str) -> Candidate {
        Candidate {
            start_lba: start,
            sectors,
            fstype,
            label: String::new(),
            gpt_type: BASIC,
            report_only: false,
            note: "",
            confidence: 1,
        }
    }

    #[test]
    fn drops_backup_superblocks() {
        let v = vec![
            cand(2048, 100_000, "ext4"),
            // same type, same size, starting inside the first: a spare copy
            cand(34_816, 100_000, "ext4"),
            // same type but a different size: a real, distinct filesystem
            cand(40_000, 50_000, "ext4"),
            // identical size but a different type, and outside: keep
            cand(500_000, 100_000, "ntfs"),
        ];
        let out = dedup_contained(v);
        let starts: Vec<u64> = out.iter().map(|c| c.start_lba).collect();
        assert_eq!(starts, vec![2048, 40_000, 500_000]);
    }

    #[test]
    fn resolves_overlaps_largest_first() {
        let v = vec![
            cand(2048, 200_000, "ntfs"),
            // ghost of an older, smaller layout inside the live one
            cand(100_000, 50_000, "fat"),
            // clear of everything
            cand(400_000, 100_000, "ext4"),
        ];
        let (keep, dropped) = resolve(&v);
        assert_eq!(
            keep.iter().map(|c| c.start_lba).collect::<Vec<_>>(),
            vec![2048, 400_000]
        );
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].start_lba, 100_000);
    }

    #[test]
    fn report_only_never_reaches_the_table() {
        let mut l = cand(2048, 100_000, "LUKS");
        l.report_only = true;
        let (keep, dropped) = resolve(&[l, cand(500_000, 1000, "ntfs")]);
        assert_eq!(keep.len(), 1);
        assert_eq!(keep[0].fstype, "ntfs");
        assert!(dropped.is_empty(), "report-only is excluded, not rejected");
    }

    #[test]
    fn corroborated_candidate_beats_a_same_sized_ghost() {
        // exactly the smoke-test disk: a volume moved forward, leaving its old
        // boot sector behind claiming the same size
        let ghost = cand(32_768, 1_015_808, "ntfs");
        let mut live = cand(237_568, 1_015_808, "ntfs");
        live.confidence = 2; // its backup boot sector is still there
        let (keep, dropped) = resolve(&[ghost, live]);
        assert_eq!(keep.len(), 1);
        assert_eq!(
            keep[0].start_lba, 237_568,
            "the ghost must not win on tie-break order"
        );
        assert_eq!(dropped[0].start_lba, 32_768);
    }

    #[test]
    fn adjacent_partitions_do_not_count_as_overlapping() {
        // one ends at 2047, the next starts at 2048
        let (keep, dropped) = resolve(&[cand(1024, 1024, "ntfs"), cand(2048, 1024, "ext4")]);
        assert_eq!(keep.len(), 2, "touching is not overlapping");
        assert!(dropped.is_empty());
    }
}
