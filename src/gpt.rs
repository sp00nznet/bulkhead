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
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
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

const PART_CRC: usize = 88;

/// Byte offsets inside a 128-byte partition entry.
const E_TYPE: usize = 0;
const E_START: usize = 32;
const E_END: usize = 40;
const E_NAME: usize = 56;

pub fn is_gpt(header: &[u8]) -> bool {
    header.len() >= MIN_HEADER && &header[..8] == SIG
}

pub fn header_size(h: &[u8]) -> usize {
    u32_at(h, HEADER_SIZE) as usize
}
pub fn entry_size(h: &[u8]) -> usize {
    u32_at(h, ENTRY_SIZE) as usize
}
pub fn entry_count(h: &[u8]) -> usize {
    u32_at(h, NUM_ENTRIES) as usize
}
pub fn entry_array_lba(h: &[u8]) -> u64 {
    u64_at(h, ENTRIES_LBA)
}
pub fn first_usable(h: &[u8]) -> u64 {
    u64_at(h, FIRST_USABLE)
}
pub fn alternate_lba(h: &[u8]) -> u64 {
    u64_at(h, ALT_LBA)
}

/// Turn a copy of the primary header into the backup one. Caller reseals.
pub fn make_backup(h: &mut [u8], last_lba: u64, entries_lba: u64) {
    put(h, MY_LBA, last_lba);
    put(h, ALT_LBA, 1);
    put(h, ENTRIES_LBA, entries_lba);
}
pub fn last_usable(h: &[u8]) -> u64 {
    u64_at(h, LAST_USABLE)
}

#[derive(Debug, PartialEq)]
pub struct Entry {
    /// 1-based, matching how every disk tool numbers partitions.
    pub number: usize,
    pub start_lba: u64,
    /// Inclusive, as GPT stores it.
    pub end_lba: u64,
    pub name: String,
}

impl Entry {
    pub fn sectors(&self) -> u64 {
        self.end_lba - self.start_lba + 1
    }
}

/// Occupied entries only, **in disk order**. An all-zero type GUID means the
/// slot is free.
///
/// Sorted because table order is not disk order and nothing good comes of
/// assuming it is: an OEM layout that adds a partition after the fact leaves
/// entry 8 sitting physically between 5 and 6, which is exactly what the
/// Lenovo test drive does. Any caller walking the list to find the gaps then
/// runs its cursor backwards and reports occupied space as free -- pointing a
/// user at 58 GB of "free" space that is a live ext4. `number` stays the
/// entry's own 1-based table index, so writes by number are unaffected.
pub fn entries(header: &[u8], array: &[u8]) -> Vec<Entry> {
    let (sz, n) = (entry_size(header), entry_count(header));
    if sz < E_NAME {
        return Vec::new();
    }
    let mut v: Vec<Entry> = (0..n)
        .filter_map(|i| {
            let e = array.get(i * sz..(i + 1) * sz)?;
            if e[E_TYPE..E_TYPE + 16].iter().all(|&b| b == 0) {
                return None;
            }
            let utf16: Vec<u16> = e[E_NAME..sz]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            Some(Entry {
                number: i + 1,
                start_lba: u64_at(e, E_START),
                end_lba: u64_at(e, E_END),
                name: String::from_utf16_lossy(&utf16),
            })
        })
        .collect();
    v.sort_by_key(|e| e.start_lba);
    v
}

/// Point entry `number` (1-based) at a new extent, keeping its length.
pub fn set_start(header: &[u8], array: &mut [u8], number: usize, start_lba: u64) -> Option<()> {
    let sz = entry_size(header);
    if number == 0 || number > entry_count(header) {
        return None;
    }
    let e = array.get_mut((number - 1) * sz..number * sz)?;
    let len = u64_at(e, E_END) - u64_at(e, E_START);
    put(e, E_START, start_lba);
    put(e, E_END, start_lba + len);
    Some(())
}

/// Recompute the entry-array CRC the header carries, then the header's own.
/// Both must be right or firmware falls back to the other copy -- or rejects
/// the disk outright.
pub fn reseal(header: &mut [u8], array: &[u8]) {
    let n = entry_count(header) * entry_size(header);
    let crc = crc32(&array[..n.min(array.len())]);
    header[PART_CRC..PART_CRC + 4].copy_from_slice(&crc.to_le_bytes());
    seal(header);
}

/// A GUID as GPT stores it: the first three fields little-endian, the last
/// eight bytes in written order. Parses the usual `{xxxxxxxx-xxxx-...}` form.
pub fn guid_bytes(s: &str) -> Option<[u8; 16]> {
    let h: Vec<u8> = s.bytes().filter(|c| c.is_ascii_hexdigit()).collect();
    if h.len() != 32 {
        return None;
    }
    let n = |i: usize| -> Option<u8> {
        let hi = (h[i] as char).to_digit(16)? as u8;
        let lo = (h[i + 1] as char).to_digit(16)? as u8;
        Some(hi << 4 | lo)
    };
    let mut raw = [0u8; 16];
    for (i, b) in raw.iter_mut().enumerate() {
        *b = n(i * 2)?;
    }
    let mut g = raw;
    g[0..4].copy_from_slice(&[raw[3], raw[2], raw[1], raw[0]]);
    g[4..6].copy_from_slice(&[raw[5], raw[4]]);
    g[6..8].copy_from_slice(&[raw[7], raw[6]]);
    Some(g)
}

pub struct NewPart {
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub start_lba: u64,
    pub end_lba: u64,
    pub name: String,
}

pub struct Table {
    /// Protective MBR, for LBA 0.
    pub mbr: Vec<u8>,
    pub primary_header: Vec<u8>,
    pub entries: Vec<u8>,
    pub backup_entries_lba: u64,
    pub backup_header: Vec<u8>,
    pub last_lba: u64,
    pub entries_lba: u64,
}

/// Build a complete GPT from scratch.
///
/// Needed because the obvious shortcut is a trap: `New-Partition` zeroes the
/// first sectors of a partition it creates, so that stale filesystem metadata
/// is not picked up. Right for making a new partition, fatal for rebuilding a
/// table over filesystems that are already there.
pub fn build(disk_size: u64, sector: u64, disk_guid: [u8; 16], parts: &[NewPart]) -> Option<Table> {
    const COUNT: u64 = 128;
    const SIZE: u64 = 128;
    if sector < 512 || disk_size < sector * 100 || parts.len() as u64 > COUNT {
        return None;
    }
    let entries_sectors = (COUNT * SIZE).div_ceil(sector);
    let last_lba = disk_size / sector - 1;
    let entries_lba = 2;
    let first_usable = 2 + entries_sectors;
    let last_usable = last_lba.checked_sub(entries_sectors + 1)?;
    if parts
        .iter()
        .any(|p| p.start_lba < first_usable || p.end_lba > last_usable)
    {
        return None;
    }

    let mut entries = vec![0u8; (entries_sectors * sector) as usize];
    for (i, p) in parts.iter().enumerate() {
        let e = &mut entries[i * SIZE as usize..(i + 1) * SIZE as usize];
        e[0..16].copy_from_slice(&p.type_guid);
        e[16..32].copy_from_slice(&p.unique_guid);
        put(e, E_START, p.start_lba);
        put(e, E_END, p.end_lba);
        for (j, c) in p.name.encode_utf16().take(35).enumerate() {
            e[E_NAME + j * 2..E_NAME + j * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
    }

    let mut h = vec![0u8; MIN_HEADER];
    h[..8].copy_from_slice(SIG);
    h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    h[HEADER_SIZE..HEADER_SIZE + 4].copy_from_slice(&(MIN_HEADER as u32).to_le_bytes());
    put(&mut h, MY_LBA, 1);
    put(&mut h, ALT_LBA, last_lba);
    put(&mut h, FIRST_USABLE, first_usable);
    put(&mut h, LAST_USABLE, last_usable);
    h[56..72].copy_from_slice(&disk_guid);
    put(&mut h, ENTRIES_LBA, entries_lba);
    h[NUM_ENTRIES..NUM_ENTRIES + 4].copy_from_slice(&(COUNT as u32).to_le_bytes());
    h[ENTRY_SIZE..ENTRY_SIZE + 4].copy_from_slice(&(SIZE as u32).to_le_bytes());
    reseal(&mut h, &entries);

    let backup_entries_lba = last_lba - entries_sectors;
    let mut b = h.clone();
    make_backup(&mut b, last_lba, backup_entries_lba);
    reseal(&mut b, &entries);

    // Protective MBR: one entry of type 0xEE covering the whole disk, so tools
    // that only understand MBR see a full disk rather than an empty one.
    let mut mbr = vec![0u8; sector as usize];
    let span = last_lba.min(0xFFFF_FFFF) as u32;
    mbr[446] = 0x00;
    mbr[447..450].copy_from_slice(&[0x00, 0x02, 0x00]);
    mbr[450] = 0xEE;
    mbr[451..454].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    mbr[454..458].copy_from_slice(&1u32.to_le_bytes());
    mbr[458..462].copy_from_slice(&span.to_le_bytes());
    mbr[510..512].copy_from_slice(&[0x55, 0xAA]);

    Some(Table {
        mbr,
        primary_header: h,
        entries,
        backup_entries_lba,
        backup_header: b,
        last_lba,
        entries_lba,
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
    fn entries_come_back_in_disk_order_not_table_order() {
        // The Lenovo test drive's real layout: a Windows 8 OEM disk whose ext4
        // partition was added later, so table entry 8 sits physically between
        // entries 5 and 6. Walking the table in index order runs the free-space
        // cursor backwards and reports a live 58 GB filesystem as free.
        let h = header(2048);
        let sz = 128usize;
        let mut array = vec![0u8; 128 * sz];
        // (table index, start_lba) -- index 3 lives between 1 and 2 on disk.
        for &(idx, start, end) in &[(1usize, 100u64, 199u64), (2, 400, 499), (3, 200, 399)] {
            let e = &mut array[(idx - 1) * sz..idx * sz];
            e[E_TYPE] = 0xAB; // any non-zero type GUID marks the slot used
            put(e, E_START, start);
            put(e, E_END, end);
        }

        let got = entries(&h, &array);
        assert_eq!(
            got.iter().map(|e| e.start_lba).collect::<Vec<_>>(),
            [100, 200, 400],
            "must be sorted by position on the disk"
        );
        assert_eq!(
            got.iter().map(|e| e.number).collect::<Vec<_>>(),
            [1, 3, 2],
            "but each keeps its own table index"
        );

        // The bug this guards: walk the sorted list and there is no gap.
        let mut pos = 100;
        for e in &got {
            assert!(e.start_lba >= pos, "cursor must never run backwards");
            pos = e.end_lba + 1;
        }
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

    /// 128 entries of 128 bytes, with `n` of them occupied back to back.
    fn array(extents: &[(u64, u64)]) -> Vec<u8> {
        let mut a = vec![0u8; 128 * 128];
        for (i, &(start, end)) in extents.iter().enumerate() {
            let e = &mut a[i * 128..(i + 1) * 128];
            e[..16].copy_from_slice(&[0xEB; 16]); // any non-zero type GUID
            put(e, E_START, start);
            put(e, E_END, end);
            let name: Vec<u16> = format!("p{}", i + 1).encode_utf16().collect();
            for (j, c) in name.iter().enumerate() {
                e[E_NAME + j * 2..E_NAME + j * 2 + 2].copy_from_slice(&c.to_le_bytes());
            }
        }
        a
    }

    #[test]
    fn reads_occupied_entries_only() {
        let h = header(100 * 1024 * 1024 / 512);
        let a = array(&[(2048, 4095), (4096, 20479)]);
        let got = entries(&h, &a);
        assert_eq!(got.len(), 2, "empty slots must not be reported");
        assert_eq!(
            got[0],
            Entry {
                number: 1,
                start_lba: 2048,
                end_lba: 4095,
                name: "p1".into()
            }
        );
        assert_eq!(got[1].number, 2);
        assert_eq!(got[1].sectors(), 16384);
    }

    #[test]
    fn moving_an_entry_keeps_its_length() {
        let h = header(100 * 1024 * 1024 / 512);
        let mut a = array(&[(2048, 4095), (4096, 20479)]);
        let before = entries(&h, &a)[1].sectors();

        set_start(&h, &mut a, 2, 40960).expect("entry 2 exists");
        let after = &entries(&h, &a)[1];
        assert_eq!(after.start_lba, 40960);
        assert_eq!(after.sectors(), before, "a move must not resize");
        assert_eq!(after.end_lba, 40960 + before - 1);
        // the untouched neighbour stays put
        assert_eq!(entries(&h, &a)[0].start_lba, 2048);

        assert!(
            set_start(&h, &mut a, 0, 100).is_none(),
            "entries are 1-based"
        );
        assert!(
            set_start(&h, &mut a, 129, 100).is_none(),
            "past the end of the array"
        );
    }

    #[test]
    fn reseal_updates_both_checksums() {
        let mut h = header(100 * 1024 * 1024 / 512)[..92].to_vec();
        let mut a = array(&[(2048, 4095)]);
        reseal(&mut h, &a);
        let array_crc = u32_at(&h, PART_CRC) as u32;

        // change the table; the header must no longer vouch for it
        set_start(&h.clone(), &mut a, 1, 8192).unwrap();
        assert_ne!(crc32(&a), array_crc, "entry CRC should track the array");

        reseal(&mut h, &a);
        assert_eq!(u32_at(&h, PART_CRC) as u32, crc32(&a));
        let stored = u32_at(&h, HEADER_CRC) as u32;
        let mut probe = h.clone();
        probe[HEADER_CRC..HEADER_CRC + 4].fill(0);
        assert_eq!(
            stored,
            crc32(&probe),
            "header CRC must cover the new entry CRC"
        );
    }

    #[test]
    fn guid_parsing_is_mixed_endian() {
        // the basic-data type GUID, as it appears on disk
        let g = guid_bytes("{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}").unwrap();
        assert_eq!(
            g,
            [
                0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26,
                0x99, 0xC7
            ]
        );
        assert_eq!(guid_bytes("not a guid"), None);
        assert_eq!(guid_bytes("{ebd0a0a2-b9e5-4433-87c0-68b6b72699}"), None);
    }

    #[test]
    fn built_table_reads_back_as_written() {
        let ty = guid_bytes("{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}").unwrap();
        let uq = guid_bytes("{11111111-2222-3333-4444-555555555555}").unwrap();
        let parts = vec![
            NewPart {
                type_guid: ty,
                unique_guid: uq,
                start_lba: 2048,
                end_lba: 200_000,
                name: "recovered1".into(),
            },
            NewPart {
                type_guid: ty,
                unique_guid: uq,
                start_lba: 200_001,
                end_lba: 400_000,
                name: "recovered2".into(),
            },
        ];
        let disk = 1u64 << 30;
        let t = build(disk, 512, uq, &parts).expect("should build");

        // parse it back with the reader used on real disks
        let got = entries(&t.primary_header, &t.entries);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].start_lba, 2048);
        assert_eq!(got[0].end_lba, 200_000);
        assert_eq!(got[0].name, "recovered1");
        assert_eq!(got[1].start_lba, 200_001);

        // both headers must self-verify and vouch for the entry array
        for h in [&t.primary_header, &t.backup_header] {
            assert_eq!(u32_at(h, PART_CRC) as u32, crc32(&t.entries));
            let mut probe = h.clone();
            probe[HEADER_CRC..HEADER_CRC + 4].fill(0);
            assert_eq!(u32_at(h, HEADER_CRC) as u32, crc32(&probe));
        }
        assert_eq!(u64_at(&t.primary_header, ALT_LBA), t.last_lba);
        assert_eq!(u64_at(&t.backup_header, MY_LBA), t.last_lba);
        assert_eq!(u64_at(&t.backup_header, ENTRIES_LBA), t.backup_entries_lba);

        // protective MBR, or MBR-only tools see an empty disk
        assert_eq!(t.mbr[450], 0xEE);
        assert_eq!(t.mbr[510..512], [0x55, 0xAA]);
    }

    #[test]
    fn build_refuses_partitions_outside_the_usable_area() {
        let ty = guid_bytes("{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}").unwrap();
        let mk = |start, end| {
            vec![NewPart {
                type_guid: ty,
                unique_guid: ty,
                start_lba: start,
                end_lba: end,
                name: String::new(),
            }]
        };
        let disk = 1u64 << 30;
        // inside the entry array
        assert!(build(disk, 512, ty, &mk(2, 1000)).is_none());
        // over the backup table at the end
        assert!(build(disk, 512, ty, &mk(2048, disk / 512 - 1)).is_none());
        assert!(build(disk, 512, ty, &mk(2048, 400_000)).is_some());
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
