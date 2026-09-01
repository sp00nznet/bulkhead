//! Read ext2/3/4 volumes, which Windows cannot.
//!
//! This is the filesystem Paragon charges per-seat to read. Nothing exotic is
//! needed to get files off one: a superblock says how the volume is laid out,
//! a table of group descriptors says where the inodes are, and each inode
//! carries either a small tree of extents or, on older volumes, a list of
//! block pointers.
//!
//! Read-only, deliberately and permanently. Writing ext4 safely means the
//! journal, and a half-understood journal is how filesystems get destroyed.
use crate::Raw;
use crate::util::{Ctx, Res};

const SUPERBLOCK_OFFSET: u64 = 1024;
const MAGIC: u16 = 0xEF53;
/// Root is always inode 2.
pub const ROOT: u64 = 2;
const EXTENTS_FLAG: u64 = 0x0008_0000;
const EXTENT_MAGIC: u16 = 0xF30A;
const INCOMPAT_64BIT: u64 = 0x80;

fn u16at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32at(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
}

/// One contiguous stretch of a file: `count` blocks starting at `block`,
/// covering logical block `logical` onward.
#[derive(Debug, PartialEq)]
pub struct Extent {
    pub logical: u64,
    pub block: u64,
    pub count: u64,
}

/// Extents directly recorded in a node, ignoring index entries.
///
/// A node is 12 bytes of header then 12 bytes per entry. Depth 0 means the
/// entries are extents; deeper means they point at further nodes, and the
/// caller has to follow them.
pub fn leaf_extents(node: &[u8]) -> Option<Vec<Extent>> {
    if node.len() < 12 || u16at(node, 0) != EXTENT_MAGIC {
        return None;
    }
    let entries = u16at(node, 2) as usize;
    if u16at(node, 6) != 0 {
        return None; // not a leaf
    }
    let mut v = Vec::new();
    for i in 0..entries {
        let e = node.get(12 + i * 12..24 + i * 12)?;
        let len = u16at(e, 4) as u64;
        // A length above 32768 marks an uninitialised extent -- allocated but
        // never written. Its contents are defined as zero, so skipping it is
        // correct and avoids reading a stale block.
        if len == 0 || len > 32768 {
            continue;
        }
        v.push(Extent {
            logical: u32at(e, 0),
            block: (u16at(e, 6) as u64) << 32 | u32at(e, 8),
            count: len,
        });
    }
    Some(v)
}

/// Where the child nodes of an interior extent node live.
pub fn index_children(node: &[u8]) -> Option<Vec<u64>> {
    if node.len() < 12 || u16at(node, 0) != EXTENT_MAGIC {
        return None;
    }
    let depth = u16at(node, 6);
    if depth == 0 {
        return None;
    }
    let entries = u16at(node, 2) as usize;
    let mut v = Vec::new();
    for i in 0..entries {
        let e = node.get(12 + i * 12..24 + i * 12)?;
        v.push((u16at(e, 8) as u64) << 32 | u32at(e, 4));
    }
    Some(v)
}

#[derive(Debug, PartialEq)]
pub struct DirEntry {
    pub inode: u64,
    pub name: String,
    pub is_dir: bool,
}

/// Directory entries packed into one block.
///
/// Each carries its own record length, and deleting a file just widens the
/// previous record to swallow it -- so a zero inode is a hole, not the end.
pub fn dir_entries(block: &[u8]) -> Vec<DirEntry> {
    let mut v = Vec::new();
    let mut off = 0usize;
    while off + 8 <= block.len() {
        let inode = u32at(block, off);
        let rec_len = u16at(block, off + 4) as usize;
        let name_len = block[off + 6] as usize;
        let file_type = block[off + 7];
        if rec_len < 8 || off + rec_len > block.len() {
            break;
        }
        if inode != 0 && name_len > 0 && off + 8 + name_len <= block.len() {
            let name = String::from_utf8_lossy(&block[off + 8..off + 8 + name_len]).into_owned();
            if name != "." && name != ".." {
                v.push(DirEntry {
                    inode,
                    name,
                    is_dir: file_type == 2,
                });
            }
        }
        off += rec_len;
    }
    v
}

/// The size ext4 actually means, from a raw inode.
///
/// The 32 bits at 0x6C are `i_size_high` **only for a regular file**. For a
/// directory, symlink or device the same field is `i_dir_acl`, and folding it
/// into the size invents terabytes -- a Debian `/usr` listed as 11256861.9 TB
/// is how this was found. Linux gates it on `S_ISREG` in `ext4_isize()` and so
/// do we.
///
/// ponytail: ignores the `largedir` feature, which also allows a high size on
/// directories. Nothing here uses a directory's byte size.
fn isize_of(ino: &[u8]) -> u64 {
    let lo = u32at(ino, 0x04);
    if u16at(ino, 0x00) & 0xF000 == 0x8000 {
        lo | (u32at(ino, 0x6C) << 32)
    } else {
        lo
    }
}

/// Bytes per group descriptor, from the superblock.
///
/// `s_feature_incompat` is at **0x60**. This read 0x64 -- `s_feature_ro_compat`
/// -- and tested 0x80 in it, which is HAS_SNAPSHOT and effectively never set,
/// so a 64-bit volume was always treated as having 32-byte descriptors. Group 0
/// still resolved (its offset is `group * desc_size` = 0 either way) and every
/// other group read a descriptor from the middle of group 0's, so every inode
/// outside the first group came back as garbage: a 5 KB file in a Debian /etc
/// reported itself as 512 MB.
fn desc_size_of(sb: &[u8]) -> u64 {
    if u32at(sb, 0x60) & INCOMPAT_64BIT == 0 {
        return 32;
    }
    match u16at(sb, 0xFE) as u64 {
        n if n >= 32 => n,
        _ => 32,
    }
}

pub struct Ext<'a> {
    disk: &'a Raw,
    base: u64,
    pub block_size: u64,
    inode_size: u64,
    inodes_per_group: u64,
    blocks_per_group: u64,
    first_data_block: u64,
    desc_size: u64,
    pub label: String,
    pub blocks: u64,
}

impl<'a> Ext<'a> {
    pub fn open(disk: &'a Raw, base: u64) -> Res<Ext<'a>> {
        let mut sb = vec![0u8; 1024];
        disk.seek(base + SUPERBLOCK_OFFSET)?;
        disk.read(&mut sb).ctx("read ext superblock")?;
        if sb.len() < 0x100 || u16at(&sb, 0x38) != MAGIC {
            return Err("not an ext2/3/4 volume".into());
        }
        let log_bs = u32at(&sb, 0x18);
        if log_bs > 16 {
            return Err("implausible block size".into());
        }
        let block_size = 1024u64 << log_bs;
        let desc_size = desc_size_of(&sb);
        let inode_size = u16at(&sb, 0x58) as u64;
        let e = Ext {
            disk,
            base,
            block_size,
            inode_size: if inode_size == 0 { 128 } else { inode_size },
            inodes_per_group: u32at(&sb, 0x28),
            blocks_per_group: u32at(&sb, 0x20),
            first_data_block: u32at(&sb, 0x14),
            desc_size,
            label: {
                let raw = &sb[0x78..0x88];
                let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
                String::from_utf8_lossy(&raw[..end]).into_owned()
            },
            blocks: u32at(&sb, 0x04),
        };
        if e.inodes_per_group == 0 || e.blocks_per_group == 0 {
            return Err("superblock geometry is not believable".into());
        }
        Ok(e)
    }

    /// Read an arbitrary range.
    ///
    /// A raw disk handle serves whole sectors from sector boundaries, and ext4
    /// structures are not obliged to be either -- a group descriptor is 32 or
    /// 64 bytes at wherever the table puts it. Read the sectors around the
    /// range and slice.
    fn read_at(&self, off: u64, len: usize) -> Res<Vec<u8>> {
        const SECTOR: u64 = 512;
        let start = off / SECTOR * SECTOR;
        let skip = (off - start) as usize;
        let total = (skip + len).div_ceil(SECTOR as usize) * SECTOR as usize;
        let mut b = vec![0u8; total];
        self.disk.seek(start)?;
        let got = self.disk.read(&mut b).ctx("read")?;
        if got < skip + len {
            return Err(format!(
                "short read at {off}: wanted {len}, got {}",
                got.saturating_sub(skip)
            )
            .into());
        }
        b.drain(..skip);
        b.truncate(len);
        Ok(b)
    }

    fn read_block(&self, block: u64) -> Res<Vec<u8>> {
        self.read_at(
            self.base + block * self.block_size,
            self.block_size as usize,
        )
    }

    /// Raw inode bytes. Inodes are numbered from 1 and live in a per-group
    /// table whose location comes from that group's descriptor.
    fn inode(&self, num: u64) -> Res<Vec<u8>> {
        if num == 0 {
            return Err("inode 0 does not exist".into());
        }
        let group = (num - 1) / self.inodes_per_group;
        let index = (num - 1) % self.inodes_per_group;

        // The group descriptor table follows the superblock's block.
        let gd_block = self.first_data_block + 1;
        let gd_off = self.base + gd_block * self.block_size + group * self.desc_size;
        let gd = self.read_at(gd_off, self.desc_size as usize)?;

        let mut table = u32at(&gd, 0x08);
        if self.desc_size >= 64 {
            table |= u32at(&gd, 0x28) << 32;
        }
        let off = self.base + table * self.block_size + index * self.inode_size;
        self.read_at(off, self.inode_size as usize)
    }

    pub fn size_of(&self, num: u64) -> Res<u64> {
        let ino = self.inode(num)?;
        Ok(isize_of(&ino))
    }

    /// Every block of a file, in logical order.
    fn blocks_of(&self, ino: &[u8]) -> Res<Vec<u64>> {
        let flags = u32at(ino, 0x20);
        let body = &ino[0x28..0x28 + 60];
        if flags & EXTENTS_FLAG == 0 {
            return Err("ext2/ext3 indirect block maps are not supported yet -- \
                        this volume predates extents"
                .into());
        }

        // Walk the tree depth-first, collecting leaves. Small files keep their
        // whole extent tree inside the inode; larger ones point outward.
        let mut out: Vec<Extent> = Vec::new();
        let mut stack = vec![body.to_vec()];
        while let Some(node) = stack.pop() {
            if let Some(mut leaves) = leaf_extents(&node) {
                out.append(&mut leaves);
            } else if let Some(children) = index_children(&node) {
                for c in children {
                    stack.push(self.read_block(c)?);
                }
            }
        }
        out.sort_by_key(|e| e.logical);

        let mut blocks = Vec::new();
        for e in out {
            for i in 0..e.count {
                blocks.push(e.block + i);
            }
        }
        Ok(blocks)
    }

    pub fn read_dir(&self, num: u64) -> Res<Vec<DirEntry>> {
        let ino = self.inode(num)?;
        let mut out = Vec::new();
        for b in self.blocks_of(&ino)? {
            out.extend(dir_entries(&self.read_block(b)?));
        }
        Ok(out)
    }

    pub fn read_file(&self, num: u64) -> Res<Vec<u8>> {
        let ino = self.inode(num)?;
        let size = isize_of(&ino);
        let mut out = Vec::new();
        for b in self.blocks_of(&ino)? {
            if out.len() as u64 >= size {
                break;
            }
            out.extend_from_slice(&self.read_block(b)?);
        }
        out.truncate(size as usize);
        Ok(out)
    }

    /// Follow a slash-separated path from the root.
    pub fn resolve(&self, path: &str) -> Res<(u64, bool)> {
        let mut ino = ROOT;
        let mut is_dir = true;
        for part in path
            .split(['/', '\\'])
            .filter(|p| !p.is_empty() && *p != ".")
        {
            let entries = self.read_dir(ino)?;
            let hit = entries
                .iter()
                .find(|e| e.name.eq_ignore_ascii_case(part))
                .ok_or_else(|| format!("{part:?} not found in {path:?}"))?;
            ino = hit.inode;
            is_dir = hit.is_dir;
        }
        Ok((ino, is_dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(depth: u16, entries: &[[u8; 12]]) -> Vec<u8> {
        let mut n = vec![0u8; 12];
        n[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
        n[2..4].copy_from_slice(&(entries.len() as u16).to_le_bytes());
        n[6..8].copy_from_slice(&depth.to_le_bytes());
        for e in entries {
            n.extend_from_slice(e);
        }
        n
    }

    fn extent(logical: u32, len: u16, block: u64) -> [u8; 12] {
        let mut e = [0u8; 12];
        e[0..4].copy_from_slice(&logical.to_le_bytes());
        e[4..6].copy_from_slice(&len.to_le_bytes());
        e[6..8].copy_from_slice(&((block >> 32) as u16).to_le_bytes());
        e[8..12].copy_from_slice(&(block as u32).to_le_bytes());
        e
    }

    #[test]
    fn reads_leaf_extents() {
        let n = node(0, &[extent(0, 4, 1000), extent(4, 2, 2000)]);
        assert_eq!(
            leaf_extents(&n).unwrap(),
            vec![
                Extent {
                    logical: 0,
                    block: 1000,
                    count: 4
                },
                Extent {
                    logical: 4,
                    block: 2000,
                    count: 2
                },
            ]
        );
        assert!(index_children(&n).is_none(), "a leaf has no children");
    }

    #[test]
    fn skips_uninitialised_extents() {
        // len > 32768 marks allocated-but-never-written; it reads as zeros,
        // so taking the block would hand back stale data.
        let n = node(0, &[extent(0, 4, 1000), extent(4, 32769, 2000)]);
        let e = leaf_extents(&n).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].block, 1000);
    }

    #[test]
    fn interior_nodes_point_at_children() {
        let mut idx = [0u8; 12];
        idx[0..4].copy_from_slice(&0u32.to_le_bytes());
        idx[4..8].copy_from_slice(&4242u32.to_le_bytes());
        let n = node(1, &[idx]);
        assert!(leaf_extents(&n).is_none(), "depth 1 is not a leaf");
        assert_eq!(index_children(&n).unwrap(), vec![4242]);
    }

    #[test]
    fn rejects_a_node_without_the_magic() {
        assert!(leaf_extents(&[0u8; 24]).is_none());
        assert!(leaf_extents(&[]).is_none());
    }

    fn dirent(inode: u32, name: &str, ftype: u8, rec_len: u16) -> Vec<u8> {
        let mut d = vec![0u8; rec_len as usize];
        d[0..4].copy_from_slice(&inode.to_le_bytes());
        d[4..6].copy_from_slice(&rec_len.to_le_bytes());
        d[6] = name.len() as u8;
        d[7] = ftype;
        d[8..8 + name.len()].copy_from_slice(name.as_bytes());
        d
    }

    /// 0x6C is i_size_high for a regular file and i_dir_acl for everything
    /// else. Reading it unconditionally reported a Debian /usr as 11 million TB.
    #[test]
    fn size_high_only_counts_for_regular_files() {
        let mut ino = vec![0u8; 128];
        let set = |ino: &mut Vec<u8>, mode: u16, lo: u32, hi: u32| {
            ino[0x00..0x02].copy_from_slice(&mode.to_le_bytes());
            ino[0x04..0x08].copy_from_slice(&lo.to_le_bytes());
            ino[0x6C..0x70].copy_from_slice(&hi.to_le_bytes());
        };

        // regular file, S_IFREG: the high half is real size
        set(&mut ino, 0o100644, 512, 1);
        assert_eq!(isize_of(&ino), (1 << 32) | 512);

        // directory, S_IFDIR: identical bytes, but 0x6C is i_dir_acl
        set(&mut ino, 0o040755, 4096, 0x8000);
        assert_eq!(isize_of(&ino), 4096);

        // symlink, S_IFLNK: same
        set(&mut ino, 0o120777, 7, 0xdead);
        assert_eq!(isize_of(&ino), 7);
    }

    /// The offset of s_feature_incompat. Reading 0x64 instead of 0x60 made
    /// every 64-bit volume look 32-bit, which silently corrupted every inode
    /// outside block group 0.
    #[test]
    fn descriptor_size_reads_the_incompat_field() {
        let mut sb = vec![0u8; 1024];
        let put32 =
            |sb: &mut Vec<u8>, o: usize, v: u32| sb[o..o + 4].copy_from_slice(&v.to_le_bytes());
        let put16 =
            |sb: &mut Vec<u8>, o: usize, v: u16| sb[o..o + 2].copy_from_slice(&v.to_le_bytes());
        put16(&mut sb, 0xFE, 64);

        // nothing set anywhere: not 64-bit, 32-byte descriptors
        assert_eq!(desc_size_of(&sb), 32);

        // 0x80 in ro_compat (0x64) is HAS_SNAPSHOT and must NOT be read as 64BIT
        put32(&mut sb, 0x64, 0x80);
        assert_eq!(desc_size_of(&sb), 32);

        // 0x80 in incompat (0x60) is 64BIT: honour s_desc_size
        put32(&mut sb, 0x60, 0x80);
        assert_eq!(desc_size_of(&sb), 64);

        // a nonsense s_desc_size still cannot go below the 32-byte minimum
        put16(&mut sb, 0xFE, 8);
        assert_eq!(desc_size_of(&sb), 32);
    }

    #[test]
    fn reads_directory_entries() {
        let mut b = Vec::new();
        b.extend(dirent(2, ".", 2, 12));
        b.extend(dirent(2, "..", 2, 12));
        b.extend(dirent(11, "lost+found", 2, 20));
        b.extend(dirent(12, "notes.txt", 1, 20));
        let e = dir_entries(&b);
        // . and .. are navigation, not contents
        assert_eq!(e.len(), 2);
        assert_eq!(
            e[0],
            DirEntry {
                inode: 11,
                name: "lost+found".into(),
                is_dir: true
            }
        );
        assert_eq!(
            e[1],
            DirEntry {
                inode: 12,
                name: "notes.txt".into(),
                is_dir: false
            }
        );
    }

    #[test]
    fn deleted_entries_are_holes_not_the_end() {
        let mut b = Vec::new();
        b.extend(dirent(0, "gone", 1, 16)); // inode 0: deleted
        b.extend(dirent(13, "after.txt", 1, 20));
        let e = dir_entries(&b);
        assert_eq!(e.len(), 1, "a zero inode must not stop the walk");
        assert_eq!(e[0].name, "after.txt");
    }

    #[test]
    fn a_bad_record_length_stops_the_walk() {
        let mut b = dirent(11, "ok", 1, 12);
        b.extend(dirent(12, "bad", 1, 12));
        // claim a record longer than the block
        b[12 + 4..12 + 6].copy_from_slice(&9999u16.to_le_bytes());
        assert_eq!(dir_entries(&b).len(), 1);
        // and a zero-length record must not loop forever
        let mut z = dirent(11, "ok", 1, 12);
        z.extend(vec![0u8; 12]);
        assert_eq!(dir_entries(&z).len(), 1);
    }
}
