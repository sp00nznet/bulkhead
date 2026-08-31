//! Read XFS volumes, which Windows cannot.
//!
//! The filesystem under most RHEL/CentOS installs and a lot of NAS boxes.
//! Big-endian on disk, unlike everything else here, and it addresses blocks by
//! a composite number that packs an allocation group index above a block index
//! -- so almost every field needs shifting apart before it means anything.
//!
//! Read-only. Same reasoning as ext4: writing means the log, and a
//! half-understood log destroys filesystems.
use crate::Raw;
use crate::util::{Ctx, Res};

const MAGIC: &[u8; 4] = b"XFSB";
const INODE_MAGIC: u16 = 0x494E; // "IN"

/// di_format: where a fork's data actually lives.
const FMT_LOCAL: u8 = 1;
const FMT_EXTENTS: u8 = 2;

/// Directory block magics. The `3` variants are v5, with a bigger header.
const DIR2_BLOCK: u32 = 0x5844_3242; // XD2B
const DIR3_BLOCK: u32 = 0x5844_4233; // XDB3
const DIR2_DATA: u32 = 0x5844_3244; // XD2D
const DIR3_DATA: u32 = 0x5844_4433; // XDD3

const FEAT_INCOMPAT_FTYPE: u32 = 0x1;
const V2_FTYPE: u32 = 0x200;

fn b16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}
fn b32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn b64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

#[derive(Debug, PartialEq)]
pub struct Extent {
    pub logical: u64,
    /// Filesystem block number: allocation group and block, packed together.
    pub start: u64,
    pub count: u64,
}

/// Unpack one 16-byte extent record.
///
/// The fields do not sit on byte boundaries: a flag, then 54 bits of logical
/// offset, 52 of start block and 21 of length, packed across both halves. The
/// start block straddles the two, which is the part that is easy to get wrong.
pub fn decode_extent(rec: &[u8]) -> Option<Extent> {
    if rec.len() < 16 {
        return None;
    }
    let hi = b64(rec, 0);
    let lo = b64(rec, 8);
    // The top bit marks an unwritten extent: allocated but never written, and
    // defined to read as zeros. Returning it would hand back stale blocks.
    if hi >> 63 != 0 {
        return None;
    }
    let count = lo & ((1 << 21) - 1);
    if count == 0 {
        return None;
    }
    Some(Extent {
        logical: (hi >> 9) & ((1u64 << 54) - 1),
        start: ((hi & 0x1FF) << 43) | (lo >> 21),
        count,
    })
}

#[derive(Debug, PartialEq)]
pub struct DirEntry {
    pub inode: u64,
    pub name: String,
    pub is_dir: bool,
}

/// Entries of a short-form directory, which lives inside the inode itself.
///
/// `i8count` being non-zero means the inode numbers are 8 bytes rather than 4;
/// `ftype` adds a type byte after each name. Both change the stride, so
/// getting either wrong turns the whole listing into noise.
pub fn shortform_entries(sf: &[u8], ftype: bool) -> Vec<DirEntry> {
    let mut v = Vec::new();
    if sf.len() < 2 {
        return v;
    }
    let count = sf[0] as usize;
    let i8count = sf[1];
    let inum_len = if i8count != 0 { 8 } else { 4 };
    let mut off = 2 + inum_len; // past the header and the parent inode
    for _ in 0..count {
        if off + 3 > sf.len() {
            break;
        }
        let namelen = sf[off] as usize;
        // namelen, then a 2-byte offset, then the name
        let name_at = off + 3;
        let after = name_at + namelen;
        let ft_at = after;
        let inum_at = after + if ftype { 1 } else { 0 };
        if inum_at + inum_len > sf.len() {
            break;
        }
        let inode = if inum_len == 8 {
            b64(sf, inum_at)
        } else {
            b32(sf, inum_at) as u64
        };
        v.push(DirEntry {
            inode,
            name: String::from_utf8_lossy(&sf[name_at..after]).into_owned(),
            is_dir: ftype && sf[ft_at] == 2,
        });
        off = inum_at + inum_len;
    }
    v
}

/// Entries in a directory data block, between `from` and `to`.
///
/// Free space inside a block is marked by a 0xFFFF tag where an inode number
/// would start, and carries its own length -- so gaps are stepped over rather
/// than parsed as entries.
pub fn data_entries(block: &[u8], from: usize, to: usize, ftype: bool) -> Vec<DirEntry> {
    let mut v = Vec::new();
    let end = to.min(block.len());
    let mut off = from;
    while off + 8 <= end {
        if b16(block, off) == 0xFFFF {
            let len = b16(block, off + 2) as usize;
            if len < 8 {
                break;
            }
            off += len;
            continue;
        }
        let inode = b64(block, off);
        let namelen = block[off + 8] as usize;
        if namelen == 0 || off + 9 + namelen > end {
            break;
        }
        let name = String::from_utf8_lossy(&block[off + 9..off + 9 + namelen]).into_owned();
        let ft = if ftype { block[off + 9 + namelen] } else { 0 };
        v.push(DirEntry {
            inode,
            name,
            is_dir: ftype && ft == 2,
        });
        // inode + namelen + name + optional ftype + 2-byte tag, to an 8-byte
        // boundary.
        let len = 8 + 1 + namelen + if ftype { 1 } else { 0 } + 2;
        off += len.div_ceil(8) * 8;
    }
    v
}

pub struct Xfs<'a> {
    disk: &'a Raw,
    base: u64,
    pub blocksize: u64,
    agblocks: u64,
    agblklog: u32,
    inopblog: u32,
    inodesize: u64,
    pub rootino: u64,
    ftype: bool,
    pub label: String,
    pub blocks: u64,
}

impl<'a> Xfs<'a> {
    pub fn open(disk: &'a Raw, base: u64) -> Res<Xfs<'a>> {
        let mut sb = vec![0u8; 512];
        disk.seek(base)?;
        disk.read(&mut sb).ctx("read XFS superblock")?;
        if &sb[0..4] != MAGIC {
            return Err("not an XFS volume".into());
        }
        let version = b16(&sb, 0x64);
        let blocksize = b32(&sb, 0x04) as u64;
        if !(512..=65536).contains(&blocksize) || !blocksize.is_power_of_two() {
            return Err("implausible XFS block size".into());
        }
        // v5 records ftype in features_incompat; v4 in the older features2.
        let ftype = if version & 0x0F >= 5 {
            b32(&sb, 0xD8) & FEAT_INCOMPAT_FTYPE != 0
        } else {
            b32(&sb, 0xC8) & V2_FTYPE != 0
        };
        let label = {
            let raw = &sb[0x6C..0x78];
            let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end]).into_owned()
        };
        Ok(Xfs {
            disk,
            base,
            blocksize,
            agblocks: b32(&sb, 0x54) as u64,
            agblklog: sb[0x7C] as u32,
            inopblog: sb[0x7B] as u32,
            inodesize: b16(&sb, 0x68) as u64,
            rootino: b64(&sb, 0x38),
            ftype,
            label,
            blocks: b64(&sb, 0x08),
        })
    }

    fn read_at(&self, off: u64, len: usize) -> Res<Vec<u8>> {
        const SECTOR: u64 = 512;
        let start = off / SECTOR * SECTOR;
        let skip = (off - start) as usize;
        let total = (skip + len).div_ceil(SECTOR as usize) * SECTOR as usize;
        let mut b = vec![0u8; total];
        self.disk.seek(start)?;
        let got = self.disk.read(&mut b).ctx("read")?;
        if got < skip + len {
            return Err(format!("short read at {off}").into());
        }
        b.drain(..skip);
        b.truncate(len);
        Ok(b)
    }

    /// A filesystem block number is an allocation group index above a block
    /// index within it, sharing one integer.
    fn fsb_offset(&self, fsb: u64) -> u64 {
        let ag = fsb >> self.agblklog;
        let blk = fsb & ((1u64 << self.agblklog) - 1);
        self.base + (ag * self.agblocks + blk) * self.blocksize
    }

    /// Inode numbers pack the same way, with a slot index below the block.
    fn inode(&self, ino: u64) -> Res<Vec<u8>> {
        let ag = ino >> (self.agblklog + self.inopblog);
        let blk = (ino >> self.inopblog) & ((1u64 << self.agblklog) - 1);
        let slot = ino & ((1u64 << self.inopblog) - 1);
        let off = self.base + (ag * self.agblocks + blk) * self.blocksize + slot * self.inodesize;
        let raw = self.read_at(off, self.inodesize as usize)?;
        if b16(&raw, 0) != INODE_MAGIC {
            return Err(format!("inode {ino} has no IN magic; wrong offset or damaged").into());
        }
        Ok(raw)
    }

    /// Where a fork's data starts inside the inode. A v3 core is 176 bytes; a
    /// v2 one ends after di_next_unlinked at 0x64, so 100 -- not 96.
    fn fork_offset(ino: &[u8]) -> usize {
        if ino[4] >= 3 { 176 } else { 100 }
    }

    /// Where the data fork ends. di_forkoff counts 8-byte units from the start
    /// of the fork area to the attribute fork; zero means there is no
    /// attribute fork and the data fork runs to the end of the inode.
    fn data_fork_end(ino: &[u8]) -> usize {
        let forkoff = ino[0x52] as usize * 8;
        if forkoff > 0 {
            (Self::fork_offset(ino) + forkoff).min(ino.len())
        } else {
            ino.len()
        }
    }

    fn extents_of(&self, ino: &[u8]) -> Res<Vec<Extent>> {
        match ino[5] {
            FMT_EXTENTS => {}
            FMT_LOCAL => return Ok(Vec::new()),
            _ => {
                return Err("XFS b-tree forks are not supported yet -- \
                             this file or directory is too large or too fragmented"
                    .into());
            }
        }
        // Read records until they run out rather than trusting a count.
        // di_nextents used to sit at 0x4c, but the NREXT64 feature -- on by
        // default in current mkfs.xfs -- moves it, and reading the old offset
        // yields zero, which silently produces an empty file. The fork's own
        // extent is the reliable bound: records are 16 bytes, and an unused
        // slot is all zeros.
        let base = Self::fork_offset(ino);
        let end = Self::data_fork_end(ino);
        let mut v = Vec::new();
        let mut at = base;
        while at + 16 <= end {
            let rec = &ino[at..at + 16];
            if rec.iter().all(|&b| b == 0) {
                break;
            }
            if let Some(e) = decode_extent(rec) {
                v.push(e);
            }
            at += 16;
        }
        v.sort_by_key(|e| e.logical);
        Ok(v)
    }

    pub fn size_of(&self, ino: u64) -> Res<u64> {
        Ok(b64(&self.inode(ino)?, 0x38))
    }

    pub fn read_file(&self, ino: u64) -> Res<Vec<u8>> {
        let raw = self.inode(ino)?;
        let size = b64(&raw, 0x38);
        if raw[5] == FMT_LOCAL {
            // Tiny files live in the inode.
            let base = Self::fork_offset(&raw);
            let end = (base + size as usize).min(raw.len());
            return Ok(raw[base..end].to_vec());
        }
        let mut out = Vec::new();
        for e in self.extents_of(&raw)? {
            if out.len() as u64 >= size {
                break;
            }
            let want = (e.count * self.blocksize) as usize;
            out.extend_from_slice(&self.read_at(self.fsb_offset(e.start), want)?);
        }
        out.truncate(size as usize);
        Ok(out)
    }

    pub fn read_dir(&self, ino: u64) -> Res<Vec<DirEntry>> {
        let raw = self.inode(ino)?;
        if raw[5] == FMT_LOCAL {
            let base = Self::fork_offset(&raw);
            return Ok(shortform_entries(&raw[base..], self.ftype));
        }

        let mut out = Vec::new();
        for e in self.extents_of(&raw)? {
            for i in 0..e.count {
                let block = self.read_at(self.fsb_offset(e.start + i), self.blocksize as usize)?;
                let magic = b32(&block, 0);
                // Header size and where the entries stop both depend on which
                // kind of directory block this is.
                let (hdr, tail) = match magic {
                    DIR2_DATA => (16, 0),
                    DIR3_DATA => (64, 0),
                    // A single-block directory keeps its lookup table at the
                    // end; entries stop before it.
                    DIR2_BLOCK => (16, 8 + b32(&block, block.len() - 8) as usize * 8),
                    DIR3_BLOCK => (64, 8 + b32(&block, block.len() - 8) as usize * 8),
                    _ => continue, // leaf or free-space block, no entries
                };
                let end = block.len().saturating_sub(tail);
                for d in data_entries(&block, hdr, end, self.ftype) {
                    if d.name != "." && d.name != ".." {
                        out.push(d);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Follow a slash-separated path from the root.
    pub fn resolve(&self, path: &str) -> Res<(u64, bool)> {
        let mut ino = self.rootino;
        let mut is_dir = true;
        for part in path
            .split(['/', '\\'])
            .filter(|p| !p.is_empty() && *p != ".")
        {
            let hit = self
                .read_dir(ino)?
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(part))
                .ok_or_else(|| format!("{part:?} not found in {path:?}"))?;
            // Without the ftype feature the entry does not say, so ask the
            // inode: mode bits 0o40000 mark a directory.
            is_dir = if self.ftype {
                hit.is_dir
            } else {
                b16(&self.inode(hit.inode)?, 2) & 0xF000 == 0x4000
            };
            ino = hit.inode;
        }
        Ok((ino, is_dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(logical: u64, start: u64, count: u64, unwritten: bool) -> [u8; 16] {
        let hi = (u64::from(unwritten) << 63) | (logical << 9) | (start >> 43);
        let lo = ((start & ((1 << 43) - 1)) << 21) | count;
        let mut r = [0u8; 16];
        r[0..8].copy_from_slice(&hi.to_be_bytes());
        r[8..16].copy_from_slice(&lo.to_be_bytes());
        r
    }

    #[test]
    fn extent_fields_unpack_across_both_halves() {
        let r = extent(7, 0x1234_5678, 9, false);
        assert_eq!(
            decode_extent(&r),
            Some(Extent {
                logical: 7,
                start: 0x1234_5678,
                count: 9
            })
        );
        // a start block big enough to occupy bits of the high word
        let r = extent(0, 0x7_FFFF_FFFF_FFFF, 1, false);
        assert_eq!(decode_extent(&r).unwrap().start, 0x7_FFFF_FFFF_FFFF);
    }

    #[test]
    fn unwritten_extents_are_skipped() {
        // allocated but never written: reads as zeros, so taking the blocks
        // would return whatever was there before
        assert_eq!(decode_extent(&extent(0, 1000, 4, true)), None);
        assert_eq!(
            decode_extent(&extent(0, 1000, 0, false)),
            None,
            "empty extent"
        );
        assert_eq!(decode_extent(&[0u8; 8]), None, "truncated record");
    }

    fn sf(entries: &[(&str, u64, u8)], i8: bool, ftype: bool) -> Vec<u8> {
        let mut v = vec![entries.len() as u8, if i8 { 1 } else { 0 }];
        v.extend_from_slice(if i8 { &[0u8; 8][..] } else { &[0u8; 4][..] }); // parent
        for (name, ino, ft) in entries {
            v.push(name.len() as u8);
            v.extend_from_slice(&[0, 0]); // offset
            v.extend_from_slice(name.as_bytes());
            if ftype {
                v.push(*ft);
            }
            if i8 {
                v.extend_from_slice(&ino.to_be_bytes());
            } else {
                v.extend_from_slice(&(*ino as u32).to_be_bytes());
            }
        }
        v
    }

    #[test]
    fn shortform_with_4_and_8_byte_inodes() {
        let d = sf(&[("etc", 133, 2), ("hosts", 200, 1)], false, true);
        let e = shortform_entries(&d, true);
        assert_eq!(e.len(), 2);
        assert_eq!(
            e[0],
            DirEntry {
                inode: 133,
                name: "etc".into(),
                is_dir: true
            }
        );
        assert_eq!(
            e[1],
            DirEntry {
                inode: 200,
                name: "hosts".into(),
                is_dir: false
            }
        );

        // the same directory with 8-byte inode numbers: a different stride,
        // and reading it with the wrong one yields nonsense
        let d = sf(&[("etc", 0x1_0000_0000, 2)], true, true);
        let e = shortform_entries(&d, true);
        assert_eq!(e[0].inode, 0x1_0000_0000);
        assert_eq!(e[0].name, "etc");
    }

    #[test]
    fn shortform_without_ftype_has_a_shorter_stride() {
        let d = sf(&[("a", 11, 0), ("bb", 22, 0)], false, false);
        let e = shortform_entries(&d, false);
        assert_eq!(e.len(), 2);
        assert_eq!((e[0].inode, e[0].name.as_str()), (11, "a"));
        assert_eq!((e[1].inode, e[1].name.as_str()), (22, "bb"));
    }

    fn entry(ino: u64, name: &str, ft: u8, ftype: bool) -> Vec<u8> {
        let mut v = ino.to_be_bytes().to_vec();
        v.push(name.len() as u8);
        v.extend_from_slice(name.as_bytes());
        if ftype {
            v.push(ft);
        }
        v.extend_from_slice(&[0, 0]); // tag
        while !v.len().is_multiple_of(8) {
            v.push(0);
        }
        v
    }

    #[test]
    fn data_block_entries_step_over_free_space() {
        let mut b = vec![0u8; 16];
        b.extend(entry(100, "one", 1, true));
        // a gap: 0xFFFF where an inode number would be, then its length
        let gap_at = b.len();
        b.extend_from_slice(&[0xFF, 0xFF, 0, 16]);
        b.extend(vec![0u8; 12]);
        assert_eq!(b.len(), gap_at + 16);
        b.extend(entry(200, "two", 1, true));

        let e = data_entries(&b, 16, b.len(), true);
        assert_eq!(e.len(), 2, "the gap must not swallow the entry after it");
        assert_eq!(e[0].inode, 100);
        assert_eq!(e[1].inode, 200);
        assert_eq!(e[1].name, "two");
    }

    #[test]
    fn a_zero_length_gap_does_not_loop() {
        let mut b = vec![0u8; 16];
        b.extend_from_slice(&[0xFF, 0xFF, 0, 0]);
        b.extend(vec![0u8; 12]);
        assert!(data_entries(&b, 16, b.len(), true).is_empty());
    }
}
