//! Read HFS+ / HFSX volumes, which Windows cannot.
//!
//! Everything on a Mac before 2017, and still what external drives and Time
//! Machine disks are formatted as. Big-endian. A volume header points at a
//! handful of special files, and the catalog -- one B-tree keyed by parent
//! folder and name -- holds every directory entry on the volume.
//!
//! Read-only.
use crate::util::{Ctx, Res};
use crate::Raw;

const HEADER_OFFSET: u64 = 1024;
/// "H+" is HFS+, "HX" is HFSX -- the case-sensitive variant. Same layout.
const SIG_HFSPLUS: u16 = 0x482B;
const SIG_HFSX: u16 = 0x4858;
/// An old HFS volume with HFS+ embedded inside it.
const SIG_HFS_WRAPPER: u16 = 0x4244;
pub const ROOT_FOLDER: u32 = 2;

const REC_FOLDER: i16 = 1;
const REC_FILE: i16 = 2;

fn b16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}
fn b32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn b64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

/// Where one fork of a file lives: up to eight extents recorded inline.
#[derive(Debug, Default, PartialEq, Clone)]
pub struct Fork {
    pub size: u64,
    pub total_blocks: u32,
    /// (start block, block count)
    pub extents: Vec<(u32, u32)>,
}

/// Parse the 80-byte fork record embedded in a volume header or catalog entry.
pub fn fork(b: &[u8]) -> Option<Fork> {
    if b.len() < 80 {
        return None;
    }
    let mut extents = Vec::new();
    for i in 0..8 {
        let start = b32(b, 16 + i * 8);
        let count = b32(b, 20 + i * 8);
        if count == 0 {
            break;
        }
        extents.push((start, count));
    }
    Some(Fork { size: b64(b, 0), total_blocks: b32(b, 12), extents })
}

/// Byte offsets of each record in a B-tree node.
///
/// They are stored at the *end* of the node, backwards: the last two bytes
/// point at record 0. Reading them forwards yields offsets into nothing.
pub fn record_offsets(node: &[u8], node_size: usize) -> Vec<usize> {
    let count = if node.len() >= 14 { b16(node, 10) as usize } else { 0 };
    let mut v = Vec::new();
    for i in 0..count {
        let at = node_size.checked_sub(2 * (i + 1));
        let Some(at) = at else { break };
        if at + 2 > node.len() {
            break;
        }
        let off = b16(node, at) as usize;
        if off < 14 || off >= node_size {
            break;
        }
        v.push(off);
    }
    v
}

/// A catalog key: the folder something lives in, and its name.
///
/// Names are UTF-16 big-endian, length-prefixed in characters rather than
/// bytes.
pub fn catalog_key(rec: &[u8]) -> Option<(u32, String, usize)> {
    if rec.len() < 8 {
        return None;
    }
    let key_len = b16(rec, 0) as usize;
    let parent = b32(rec, 2);
    let name_len = b16(rec, 6) as usize;
    let end = 8 + name_len * 2;
    if end > rec.len() || end > key_len + 2 {
        return None;
    }
    let utf16: Vec<u16> = rec[8..end].chunks_exact(2).map(|c| b16(c, 0)).collect();
    // The record data follows the key, aligned to a two-byte boundary.
    let data_at = (2 + key_len).div_ceil(2) * 2;
    Some((parent, String::from_utf16_lossy(&utf16), data_at))
}

#[derive(Debug, PartialEq)]
pub struct DirEntry {
    pub id: u32,
    pub name: String,
    pub is_dir: bool,
}

pub struct Hfs<'a> {
    disk: &'a Raw,
    base: u64,
    pub block_size: u64,
    catalog: Fork,
    node_size: usize,
    first_leaf: u32,
    pub blocks: u32,
    pub case_sensitive: bool,
}

impl<'a> Hfs<'a> {
    pub fn open(disk: &'a Raw, base: u64) -> Res<Hfs<'a>> {
        let mut vh = vec![0u8; 1024];
        disk.seek(base + HEADER_OFFSET)?;
        disk.read(&mut vh).ctx("read HFS+ volume header")?;
        let sig = b16(&vh, 0);
        if sig == SIG_HFS_WRAPPER {
            return Err("this is an old HFS volume wrapping HFS+; \
                        the embedded volume is not located yet"
                .into());
        }
        if sig != SIG_HFSPLUS && sig != SIG_HFSX {
            return Err("not an HFS+ volume".into());
        }
        let block_size = b32(&vh, 0x28) as u64;
        if !(512..=1 << 20).contains(&block_size) || !block_size.is_power_of_two() {
            return Err("implausible HFS+ block size".into());
        }
        let catalog = fork(&vh[0x110..0x110 + 80]).ok_or("catalog fork is unreadable")?;
        if catalog.extents.is_empty() {
            return Err("catalog fork has no extents".into());
        }

        let mut h = Hfs {
            disk,
            base,
            block_size,
            catalog,
            node_size: 0,
            first_leaf: 0,
            blocks: b32(&vh, 0x2C),
            case_sensitive: sig == SIG_HFSX,
        };

        // Node 0 of the catalog is its header node, and the only way to learn
        // how big the other nodes are. Read enough of it without knowing yet.
        let head = h.catalog_bytes(0, 512)?;
        h.node_size = b16(&head, 14 + 18) as usize;
        h.first_leaf = b32(&head, 14 + 10);
        if !(512..=65536).contains(&h.node_size) || !h.node_size.is_power_of_two() {
            return Err(format!("implausible catalog node size {}", h.node_size).into());
        }
        Ok(h)
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

    /// Read from a fork by logical offset, following its extents.
    fn fork_read(&self, f: &Fork, off: u64, len: usize) -> Res<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        let mut pos = off;
        let mut left = len;
        while left > 0 {
            // Find which extent covers `pos`.
            let mut walked = 0u64;
            let mut found = None;
            for &(start, count) in &f.extents {
                let span = count as u64 * self.block_size;
                if pos < walked + span {
                    found = Some((start as u64 * self.block_size + (pos - walked), walked + span - pos));
                    break;
                }
                walked += span;
            }
            let Some((at, avail)) = found else {
                // Past the inline extents. A badly fragmented fork continues in
                // the extents overflow file, which is not followed yet.
                return Err("fork continues in the extents overflow file, \
                            which is not supported yet"
                    .into());
            };
            let take = (avail as usize).min(left);
            out.extend_from_slice(&self.read_at(self.base + at, take)?);
            pos += take as u64;
            left -= take;
        }
        Ok(out)
    }

    fn catalog_bytes(&self, node: u32, len: usize) -> Res<Vec<u8>> {
        let size = if self.node_size == 0 { len } else { self.node_size };
        self.fork_read(&self.catalog, node as u64 * size as u64, len)
    }

    /// Every entry in a folder.
    ///
    /// ponytail: walks the leaf chain rather than descending the B-tree, so a
    /// listing costs a pass over the whole catalog. Correct without
    /// implementing HFS+'s case-folding key comparison, which is its own
    /// standard. Descend the tree if catalogue size ever makes this hurt.
    pub fn read_dir(&self, folder: u32) -> Res<Vec<DirEntry>> {
        let mut out = Vec::new();
        let mut node = self.first_leaf;
        let mut guard = 0u32;
        while node != 0 {
            guard += 1;
            if guard > 1_000_000 {
                return Err("catalog leaf chain does not end; the tree is damaged".into());
            }
            let buf = self.catalog_bytes(node, self.node_size)?;
            for off in record_offsets(&buf, self.node_size) {
                let Some((parent, name, data_at)) = catalog_key(&buf[off..]) else { continue };
                if parent != folder {
                    continue;
                }
                let d = off + data_at;
                if d + 10 > buf.len() {
                    continue;
                }
                let kind = b16(&buf, d) as i16;
                match kind {
                    REC_FOLDER => out.push(DirEntry { id: b32(&buf, d + 8), name, is_dir: true }),
                    REC_FILE => out.push(DirEntry { id: b32(&buf, d + 8), name, is_dir: false }),
                    _ => {} // thread records, which map an id back to its parent
                }
            }
            node = b32(&buf, 0); // fLink: the next leaf
        }
        Ok(out)
    }

    /// The data fork of a file, found by its catalog id.
    fn file_fork(&self, id: u32) -> Res<Fork> {
        let mut node = self.first_leaf;
        while node != 0 {
            let buf = self.catalog_bytes(node, self.node_size)?;
            for off in record_offsets(&buf, self.node_size) {
                let Some((_, _, data_at)) = catalog_key(&buf[off..]) else { continue };
                let d = off + data_at;
                if d + 168 > buf.len() || b16(&buf, d) as i16 != REC_FILE || b32(&buf, d + 8) != id {
                    continue;
                }
                // dataFork sits 88 bytes into a catalog file record.
                return fork(&buf[d + 88..d + 168]).ok_or_else(|| "unreadable data fork".into());
            }
            node = b32(&buf, 0);
        }
        Err(format!("no file record for id {id}").into())
    }

    pub fn size_of(&self, id: u32) -> Res<u64> {
        Ok(self.file_fork(id).map(|f| f.size).unwrap_or(0))
    }

    pub fn read_file(&self, id: u32) -> Res<Vec<u8>> {
        let f = self.file_fork(id)?;
        if f.size == 0 {
            return Ok(Vec::new());
        }
        self.fork_read(&f, 0, f.size as usize)
    }

    pub fn resolve(&self, path: &str) -> Res<(u32, bool)> {
        let mut id = ROOT_FOLDER;
        let mut is_dir = true;
        for part in path.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".") {
            let hit = self
                .read_dir(id)?
                .into_iter()
                .find(|e| {
                    if self.case_sensitive { e.name == part } else { e.name.eq_ignore_ascii_case(part) }
                })
                .ok_or_else(|| format!("{part:?} not found in {path:?}"))?;
            id = hit.id;
            is_dir = hit.is_dir;
        }
        Ok((id, is_dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_stops_at_the_first_empty_extent() {
        let mut b = vec![0u8; 80];
        b[0..8].copy_from_slice(&4096u64.to_be_bytes()); // logical size
        b[12..16].copy_from_slice(&3u32.to_be_bytes()); // total blocks
        b[16..20].copy_from_slice(&100u32.to_be_bytes());
        b[20..24].copy_from_slice(&2u32.to_be_bytes());
        b[24..28].copy_from_slice(&200u32.to_be_bytes());
        b[28..32].copy_from_slice(&1u32.to_be_bytes());
        // third extent left zero: the rest are unused, not extents at block 0
        let f = fork(&b).unwrap();
        assert_eq!(f.size, 4096);
        assert_eq!(f.extents, vec![(100, 2), (200, 1)]);
    }

    #[test]
    fn record_offsets_are_stored_backwards() {
        let node_size = 512;
        let mut n = vec![0u8; node_size];
        n[10..12].copy_from_slice(&3u16.to_be_bytes()); // numRecords
        // last two bytes point at record 0, and so on inwards
        for (i, off) in [14u16, 60, 120].iter().enumerate() {
            let at = node_size - 2 * (i + 1);
            n[at..at + 2].copy_from_slice(&off.to_be_bytes());
        }
        assert_eq!(record_offsets(&n, node_size), vec![14, 60, 120]);
    }

    #[test]
    fn record_offsets_reject_nonsense() {
        let node_size = 512;
        let mut n = vec![0u8; node_size];
        n[10..12].copy_from_slice(&2u16.to_be_bytes());
        // first offset points into the node descriptor, which cannot hold a
        // record; stop rather than parse the header as one
        let at = node_size - 2;
        n[at..at + 2].copy_from_slice(&4u16.to_be_bytes());
        assert!(record_offsets(&n, node_size).is_empty());
    }

    fn key(parent: u32, name: &str) -> Vec<u8> {
        let utf16: Vec<u16> = name.encode_utf16().collect();
        let key_len = 6 + utf16.len() * 2; // parent + name length + name
        let mut v = (key_len as u16).to_be_bytes().to_vec();
        v.extend_from_slice(&parent.to_be_bytes());
        v.extend_from_slice(&(utf16.len() as u16).to_be_bytes());
        for c in utf16 {
            v.extend_from_slice(&c.to_be_bytes());
        }
        v
    }

    #[test]
    fn catalog_key_reads_parent_and_name() {
        let k = key(2, "Documents");
        let (parent, name, data_at) = catalog_key(&k).unwrap();
        assert_eq!(parent, 2);
        assert_eq!(name, "Documents");
        // data follows the key on a two-byte boundary
        assert_eq!(data_at, k.len());
    }

    #[test]
    fn catalog_key_handles_non_ascii() {
        let k = key(5, "Photos \u{2013} 2024");
        let (_, name, _) = catalog_key(&k).unwrap();
        assert_eq!(name, "Photos \u{2013} 2024");
    }

    #[test]
    fn catalog_key_rejects_a_name_longer_than_the_record() {
        let mut k = key(2, "x");
        k[6..8].copy_from_slice(&999u16.to_be_bytes()); // claim a huge name
        assert!(catalog_key(&k).is_none());
        assert!(catalog_key(&[0u8; 4]).is_none());
    }
}
