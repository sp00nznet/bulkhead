//! Just enough GPT to move a partition table onto a differently-sized disk.
//!
//! Restoring to an identical drive is a straight byte copy. Restoring to a
//! *bigger* one -- which is most of why people keep images -- leaves the header
//! claiming a disk that ends where the old one did, and the backup table
//! stranded in the middle. Both are checksummed, so neither can be nudged
//! without recomputing CRCs.

/// GPT header field offsets. Named because the numbers alone are unreadable.
const SIG: &[u8; 8] = b"EFI PART";
const HEADER_SIZE: usize = 12;
const HEADER_CRC: usize = 16;
const MY_LBA: usize = 24;
const ALT_LBA: usize = 32;
const FIRST_USABLE: usize = 40;
const LAST_USABLE: usize = 48;
const ENTRIES_LBA: usize = 72;
const NUM_ENTRIES: usize = 80;
const ENTRY_SIZE: usize = 84;
const MIN_HEADER: usize = 92;

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn u32_at(h: &[u8], off: usize) -> u64 {
    u32::from_le_bytes(h[off..off + 4].try_into().unwrap()) as u64
}

fn u64_at(h: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(h[off..off + 8].try_into().unwrap())
}

fn put(h: &mut [u8], off: usize, v: u64) {
    h[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// The header carries a CRC of itself, computed with that field zeroed.
fn seal(h: &mut [u8]) {
    h[HEADER_CRC..HEADER_CRC + 4].fill(0);
    let c = crc32(h);
    h[HEADER_CRC..HEADER_CRC + 4].copy_from_slice(&c.to_le_bytes());
}

pub struct Fixup {
    /// Rewritten primary header, for LBA 1.
    pub primary: Vec<u8>,
    /// Rewritten backup header, for the last LBA.
    pub backup: Vec<u8>,
    /// Where the backup copy of the entry array belongs.
    pub entries_lba: u64,
    /// Where the entry array is read from now.
    pub source_entries_lba: u64,
    pub entries_bytes: u64,
    pub last_lba: u64,
}

/// Rewrite a GPT to describe a disk of `disk_size` bytes.
///
/// `primary` is the sector at LBA 1 as read off the disk. Returns `None` if it
/// is not a GPT, or if the geometry does not add up -- callers treat that as
/// "leave the table alone" rather than an error, since an MBR image restored
/// byte-for-byte is still perfectly valid.
pub fn relocate(primary: &[u8], disk_size: u64, sector: u64) -> Option<Fixup> {
    if primary.len() < MIN_HEADER || &primary[..8] != SIG || sector == 0 {
        return None;
    }
    let hdr_size = u32_at(primary, HEADER_SIZE) as usize;
    if !(MIN_HEADER..=primary.len()).contains(&hdr_size) {
        return None;
    }

    let entries_bytes = u32_at(primary, NUM_ENTRIES) * u32_at(primary, ENTRY_SIZE);
    if entries_bytes == 0 {
        return None;
    }
    let entries_sectors = entries_bytes.div_ceil(sector);

    let last_lba = disk_size / sector - 1;
    // The backup header sits on the last LBA and its entry array immediately
    // before it; everything below that is usable.
    let last_usable = last_lba.checked_sub(entries_sectors + 1)?;
    if last_usable <= u64_at(primary, FIRST_USABLE) {
        return None;
    }

    let mut p = primary[..hdr_size].to_vec();
    put(&mut p, ALT_LBA, last_lba);
    put(&mut p, LAST_USABLE, last_usable);
    seal(&mut p);

    let entries_lba = last_lba - entries_sectors;
    let mut b = p.clone();
    put(&mut b, MY_LBA, last_lba);
    put(&mut b, ALT_LBA, 1);
    put(&mut b, ENTRIES_LBA, entries_lba);
    seal(&mut b);

    Some(Fixup {
        primary: p,
        backup: b,
        entries_lba,
        source_entries_lba: u64_at(primary, ENTRIES_LBA),
        entries_bytes,
        last_lba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 512-byte LBA-1 sector for a 100 MiB disk, 128 entries of 128 bytes.
    fn header(disk_sectors: u64) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[..8].copy_from_slice(SIG);
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[HEADER_SIZE..HEADER_SIZE + 4].copy_from_slice(&92u32.to_le_bytes());
        put(&mut h, MY_LBA, 1);
        put(&mut h, ALT_LBA, disk_sectors - 1);
        put(&mut h, FIRST_USABLE, 34);
        put(&mut h, LAST_USABLE, disk_sectors - 34);
        put(&mut h, ENTRIES_LBA, 2);
        h[NUM_ENTRIES..NUM_ENTRIES + 4].copy_from_slice(&128u32.to_le_bytes());
        h[ENTRY_SIZE..ENTRY_SIZE + 4].copy_from_slice(&128u32.to_le_bytes());
        let mut hdr = h[..92].to_vec();
        seal(&mut hdr);
        h[..92].copy_from_slice(&hdr);
        h
    }

    #[test]
    fn crc_matches_the_standard_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn moves_the_table_to_the_end_of_a_bigger_disk() {
        // imaged from 100 MiB, restored onto 500 MiB
        let src = header(100 * 1024 * 1024 / 512);
        let f = relocate(&src, 500 * 1024 * 1024, 512).expect("should relocate");

        let new_last = 500 * 1024 * 1024 / 512 - 1;
        assert_eq!(f.last_lba, new_last);
        // 128 * 128 bytes = 32 sectors of entries, then the header itself
        assert_eq!(f.entries_lba, new_last - 32);
        assert_eq!(u64_at(&f.primary, ALT_LBA), new_last);
        assert_eq!(u64_at(&f.primary, LAST_USABLE), new_last - 33);
        assert_eq!(f.source_entries_lba, 2);

        // backup describes itself as living at the end, pointing back to LBA 1
        assert_eq!(u64_at(&f.backup, MY_LBA), new_last);
        assert_eq!(u64_at(&f.backup, ALT_LBA), 1);
        assert_eq!(u64_at(&f.backup, ENTRIES_LBA), new_last - 32);

        // both must self-verify, or the firmware rejects the table
        for h in [&f.primary, &f.backup] {
            let stored = u32_at(h, HEADER_CRC) as u32;
            let mut probe = h.clone();
            probe[HEADER_CRC..HEADER_CRC + 4].fill(0);
            assert_eq!(stored, crc32(&probe));
        }
    }

    #[test]
    fn declines_what_it_should_not_touch() {
        let src = header(100 * 1024 * 1024 / 512);
        // not a GPT -- an MBR image restores verbatim and stays valid
        assert!(relocate(&[0u8; 512], 500 << 20, 512).is_none());
        // target too small to hold even the backup table
        assert!(relocate(&src, 4096, 512).is_none());
        // nonsense sector size
        assert!(relocate(&src, 500 << 20, 0).is_none());
    }
}
