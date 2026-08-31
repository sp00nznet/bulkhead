//! The MBR partition table: four entries in the boot sector, and a chain.
//!
//! Read-only on purpose. Everything that *writes* a table in bulkhead writes
//! GPT, and the reason to read MBR is to stop refusing disks: `part list` and
//! `identify` used to answer "not a GPT disk (MBR disks are not supported
//! yet)" and stop there, which is a poor answer when someone is holding the
//! only copy of their data.
//!
//! Entries come back as `gpt::Entry`. The two tables record a partition very
//! differently but describe the same thing -- a number, a place, a length and
//! something to call it -- so sharing the shape means `part list` and
//! `identify` need no idea which kind of disk they are looking at.
use crate::Raw;
use crate::gpt::Entry;
use crate::util::Res;

const SIG_AT: usize = 510;
const TABLE_AT: usize = 446;
const ENTRY_LEN: usize = 16;

/// Where the extended chain lives. 0x05 and 0x85 are CHS-addressed, 0x0F is
/// the LBA form; all three are containers rather than filesystems.
const EXTENDED: [u8; 3] = [0x05, 0x0F, 0x85];
/// The single entry a GPT disk's protective MBR carries.
const GPT_PROTECTIVE: u8 = 0xEE;

/// A boot sector ends in 0x55AA. Necessary, not sufficient -- plenty of things
/// end that way -- so callers should try GPT first and fall back to this.
pub fn is_mbr(lba0: &[u8]) -> bool {
    lba0.len() > SIG_AT + 1 && lba0[SIG_AT] == 0x55 && lba0[SIG_AT + 1] == 0xAA
}

/// Is this the protective MBR that fronts a GPT disk? If so and the GPT itself
/// would not read, the table is damaged rather than absent -- a difference
/// worth telling someone about before they go looking for their partitions.
pub fn is_protective(lba0: &[u8]) -> bool {
    slots(lba0).iter().any(|s| s.kind == GPT_PROTECTIVE)
}

/// One raw table slot, before it is given a number or a name.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Slot {
    kind: u8,
    start: u32,
    sectors: u32,
}

/// The four slots of one sector's table, empty ones dropped.
fn slots(sec: &[u8]) -> Vec<Slot> {
    (0..4)
        .filter_map(|i| {
            let e = sec.get(TABLE_AT + i * ENTRY_LEN..TABLE_AT + (i + 1) * ENTRY_LEN)?;
            let s = Slot {
                kind: e[4],
                start: u32::from_le_bytes([e[8], e[9], e[10], e[11]]),
                sectors: u32::from_le_bytes([e[12], e[13], e[14], e[15]]),
            };
            // Type 0 is an unused slot; a zero length is a slot that says
            // nothing, and both are worth skipping rather than reporting.
            (s.kind != 0 && s.sectors != 0).then_some(s)
        })
        .collect()
}

/// What a type byte means, for the handful worth naming. Anything else prints
/// its number -- better than guessing, and the number is searchable.
fn name_of(kind: u8) -> String {
    let s = match kind {
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x05 | 0x0F | 0x85 => "Extended",
        0x07 => "NTFS or exFAT",
        0x0B | 0x0C => "FAT32",
        0x11 | 0x14 | 0x16 | 0x1B | 0x1C | 0x1E => "Hidden FAT",
        0x27 => "Windows recovery",
        0x42 => "Windows dynamic",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0x8E => "Linux LVM",
        0xA5 | 0xA6 | 0xA9 => "BSD",
        0xAF => "HFS+",
        0xEE => "GPT protective",
        0xEF => "EFI system",
        0xFD => "Linux RAID",
        _ => return format!("type {kind:#04x}"),
    };
    s.into()
}

/// Read the table, following the extended chain.
///
/// `read` is handed a byte offset and returns that sector, which keeps the
/// chain walk testable without a disk.
pub fn parse(lba0: &[u8], sector: u64, mut read: impl FnMut(u64) -> Res<Vec<u8>>) -> Vec<Entry> {
    let mut out = Vec::new();
    let primaries = slots(lba0);

    // A protective MBR is not a partition table, it is a "do not touch me"
    // sign for tools that cannot read GPT. Reporting its one entry as a
    // partition spanning the disk would be actively misleading.
    if primaries.iter().any(|s| s.kind == GPT_PROTECTIVE) {
        return out;
    }

    let mut extended = None;
    for (i, s) in primaries.iter().enumerate() {
        if EXTENDED.contains(&s.kind) {
            // The container itself is not a partition anyone can use, so it is
            // not listed -- only the logical partitions inside it are.
            extended = Some(*s);
            continue;
        }
        out.push(Entry {
            number: i + 1,
            start_lba: s.start as u64,
            end_lba: s.start as u64 + s.sectors as u64 - 1,
            name: name_of(s.kind),
        });
    }

    // Logical partitions live in a linked list of EBRs. In each one the first
    // slot is the partition, its start relative to that EBR; the second is the
    // link to the next EBR, relative to the extended container.
    if let Some(ext) = extended {
        let base = ext.start as u64;
        let mut next = base;
        let mut number = 5; // logicals are numbered from 5 by convention
        let mut seen = Vec::new();
        while !seen.contains(&next) && seen.len() < 128 {
            seen.push(next);
            let Ok(sec) = read(next * sector) else { break };
            if !is_mbr(&sec) {
                break;
            }
            let here = slots(&sec);
            let Some(part) = here.iter().find(|s| !EXTENDED.contains(&s.kind)) else {
                break;
            };
            let start = next + part.start as u64;
            out.push(Entry {
                number,
                start_lba: start,
                end_lba: start + part.sectors as u64 - 1,
                name: name_of(part.kind),
            });
            number += 1;
            match here.iter().find(|s| EXTENDED.contains(&s.kind)) {
                Some(link) => next = base + link.start as u64,
                None => break,
            }
        }
    }

    out.sort_by_key(|e| e.start_lba);
    out
}

/// The partitions on an MBR disk, in disk order.
pub fn entries(disk: &Raw, lba0: &[u8], sector: u64) -> Vec<Entry> {
    parse(lba0, sector, |at| {
        let mut b = vec![0u8; sector as usize];
        disk.seek(at)?;
        disk.read(&mut b)?;
        Ok(b)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A 512-byte sector carrying `parts` in its table, with the boot mark.
    fn sector(parts: &[(u8, u32, u32)]) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[SIG_AT] = 0x55;
        s[SIG_AT + 1] = 0xAA;
        for (i, (kind, start, sectors)) in parts.iter().enumerate() {
            let e = &mut s[TABLE_AT + i * ENTRY_LEN..TABLE_AT + (i + 1) * ENTRY_LEN];
            e[4] = *kind;
            e[8..12].copy_from_slice(&start.to_le_bytes());
            e[12..16].copy_from_slice(&sectors.to_le_bytes());
        }
        s
    }

    fn no_disk(_: u64) -> Res<Vec<u8>> {
        Err("should not have been asked for a sector".into())
    }

    #[test]
    fn reads_the_primaries_and_skips_empty_slots() {
        let s = sector(&[(0x07, 2048, 1000), (0x83, 4096, 2000)]);
        let got = parse(&s, 512, no_disk);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].number, 1);
        assert_eq!((got[0].start_lba, got[0].end_lba), (2048, 3047));
        assert_eq!(got[0].name, "NTFS or exFAT");
        assert_eq!(got[1].name, "Linux");
    }

    #[test]
    fn a_protective_mbr_is_not_a_partition_table() {
        // Every GPT disk has one of these. Listing its single entry as a
        // partition covering the whole disk would be a lie with consequences.
        let s = sector(&[(0xEE, 1, 0xFFFF_FFFF)]);
        assert!(parse(&s, 512, no_disk).is_empty());
    }

    #[test]
    fn walks_the_extended_chain() {
        // Container at 10000. Two logicals: the first EBR sits at the
        // container start, the second is linked relative to it.
        let lba0 = sector(&[(0x07, 2048, 1000), (0x05, 10_000, 50_000)]);
        let ebr1 = sector(&[(0x83, 63, 4_000), (0x05, 5_000, 20_000)]);
        let ebr2 = sector(&[(0x82, 63, 3_000)]);
        let disk: HashMap<u64, Vec<u8>> = HashMap::from([
            (10_000 * 512, ebr1),
            (15_000 * 512, ebr2), // 10000 + 5000, relative to the container
        ]);

        let got = parse(&lba0, 512, |at| {
            disk.get(&at)
                .cloned()
                .ok_or_else(|| "no such sector".into())
        });

        assert_eq!(got.len(), 3, "one primary and two logicals");
        // The extended container itself is never listed as a partition.
        assert!(got.iter().all(|e| e.name != "Extended"));
        assert_eq!(got[1].start_lba, 10_063, "first logical: EBR + its offset");
        assert_eq!(got[1].name, "Linux");
        assert_eq!(got[2].start_lba, 15_063, "second logical, via the link");
        assert_eq!(got[2].name, "Linux swap");
        assert_eq!(
            (got[1].number, got[2].number),
            (5, 6),
            "logicals start at 5"
        );
    }

    #[test]
    fn a_chain_that_points_at_itself_terminates() {
        // Corrupt tables do this, and an infinite loop while reading someone's
        // failing disk is the worst possible response to it.
        let lba0 = sector(&[(0x05, 100, 5_000)]);
        let loops = sector(&[(0x83, 63, 500), (0x05, 0, 5_000)]); // link back to base
        let disk: HashMap<u64, Vec<u8>> = HashMap::from([(100 * 512, loops)]);
        let got = parse(&lba0, 512, |at| {
            disk.get(&at)
                .cloned()
                .ok_or_else(|| "no such sector".into())
        });
        assert_eq!(got.len(), 1, "the one logical, and no spinning");
    }

    #[test]
    fn junk_is_not_a_boot_sector() {
        assert!(!is_mbr(&[0u8; 512]));
        assert!(!is_mbr(&[]));
    }
}
