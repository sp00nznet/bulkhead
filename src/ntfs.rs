//! Just enough NTFS to find deleted files and read their contents back.
//!
//! Deleting a file on NTFS clears one flag in its MFT record and marks its
//! clusters free. The record, the name and the map of where the data lives all
//! survive until something reuses them -- which is why undelete works at all,
//! and why it stops working the moment you keep using the volume.
use crate::Raw;
use crate::util::{Ctx, Res};

/// Every MFT record starts with this.
const MAGIC: &[u8; 4] = b"FILE";

// MFT record header
const R_USA_OFF: usize = 0x04;
const R_USA_CNT: usize = 0x06;
const R_ATTRS: usize = 0x14;
const R_FLAGS: usize = 0x16;

// Attribute header
const A_TYPE: usize = 0x00;
const A_LEN: usize = 0x04;
const A_NONRES: usize = 0x08;
const A_RES_LEN: usize = 0x10;
const A_RES_OFF: usize = 0x14;
const A_RUN_OFF: usize = 0x20;
const A_REAL_SIZE: usize = 0x30;

const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

fn u16at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Undo the update-sequence fixup.
///
/// NTFS overwrites the last two bytes of every sector in a record with a
/// counter, keeping the originals in an array in the header. It is a torn-write
/// detector. The record is unusable until they are put back, and the mismatch
/// check is a free integrity test: a record whose sectors do not all carry the
/// same counter was half-written or has been partly overwritten.
pub fn apply_fixup(rec: &mut [u8], sector_size: usize) -> bool {
    if rec.len() < R_ATTRS || sector_size < 4 {
        return false;
    }
    let off = u16at(rec, R_USA_OFF) as usize;
    let count = u16at(rec, R_USA_CNT) as usize;
    if count == 0 || off + count * 2 > rec.len() {
        return false;
    }
    let stamp = [rec[off], rec[off + 1]];
    // count includes the stamp itself, so there are count-1 sectors.
    for i in 0..count - 1 {
        let tail = (i + 1) * sector_size;
        if tail > rec.len() {
            return false;
        }
        if rec[tail - 2..tail] != stamp {
            return false;
        }
        let src = off + 2 + i * 2;
        rec[tail - 2] = rec[src];
        rec[tail - 1] = rec[src + 1];
    }
    true
}

/// Decode a data run list into absolute `(lcn, clusters)` extents.
///
/// Runs are a chain of deltas: each entry's offset is relative to the previous
/// run's start, and can be negative. A zero-length offset field means a sparse
/// run -- a hole with no clusters behind it, which must not shift the position.
pub fn decode_runs(b: &[u8]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut lcn: i64 = 0;
    let mut i = 0usize;
    while i < b.len() && b[i] != 0 {
        let len_sz = (b[i] & 0x0F) as usize;
        let off_sz = (b[i] >> 4) as usize;
        i += 1;
        if len_sz == 0 || len_sz > 8 || off_sz > 8 || i + len_sz + off_sz > b.len() {
            break;
        }

        let mut len = 0u64;
        for k in 0..len_sz {
            len |= (b[i + k] as u64) << (k * 8);
        }
        i += len_sz;

        if off_sz == 0 {
            // Sparse: no clusters allocated, and the running LCN stays put.
            i += off_sz;
            continue;
        }
        let mut delta = 0i64;
        for k in 0..off_sz {
            delta |= (b[i + k] as i64) << (k * 8);
        }
        // Sign-extend from the field's actual width.
        let bits = off_sz * 8;
        if bits < 64 && delta & (1 << (bits - 1)) != 0 {
            delta -= 1i64 << bits;
        }
        i += off_sz;

        lcn += delta;
        if lcn < 0 || len == 0 {
            break;
        }
        out.push((lcn as u64, len));
    }
    out
}

/// Walk a record's attributes, yielding `(type, slice)` for each.
pub fn attrs(rec: &[u8]) -> Vec<(u32, &[u8])> {
    let mut out = Vec::new();
    if rec.len() < R_ATTRS + 2 {
        return out;
    }
    let mut off = u16at(rec, R_ATTRS) as usize;
    while off + 8 <= rec.len() {
        let ty = u32at(rec, off + A_TYPE);
        if ty == ATTR_END {
            break;
        }
        let len = u32at(rec, off + A_LEN) as usize;
        if len < 16 || off + len > rec.len() {
            break;
        }
        out.push((ty, &rec[off..off + len]));
        off += len;
    }
    out
}

/// The name from a $FILE_NAME attribute, skipping DOS 8.3 aliases.
fn file_name(a: &[u8]) -> Option<String> {
    if a[A_NONRES] != 0 {
        return None;
    }
    let vo = u16at(a, A_RES_OFF) as usize;
    let v = a.get(vo..)?;
    if v.len() < 0x42 {
        return None;
    }
    // Namespace 2 is the 8.3 alias of a name stored elsewhere in the record.
    if v[0x41] == 2 {
        return None;
    }
    let n = v[0x40] as usize;
    let bytes = v.get(0x42..0x42 + n * 2)?;
    let utf16: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(String::from_utf16_lossy(&utf16))
}

#[derive(Debug)]
pub struct Deleted {
    pub name: String,
    pub size: u64,
    /// Small files live inside the MFT record itself and come back whole.
    pub resident: Option<Vec<u8>>,
    pub runs: Vec<(u64, u64)>,
}

pub struct Ntfs<'a> {
    disk: &'a Raw,
    /// Byte offset of the volume within whatever `disk` is.
    base: u64,
    pub cluster: u64,
    rec_size: u64,
    sector: usize,
    /// Where the MFT itself lives; it can be fragmented like any other file.
    mft: Vec<(u64, u64)>,
}

impl<'a> Ntfs<'a> {
    pub fn open(disk: &'a Raw, base: u64) -> Res<Ntfs<'a>> {
        let mut bs = vec![0u8; 512];
        disk.seek(base)?;
        disk.read(&mut bs).ctx("read boot sector")?;
        if &bs[3..11] != b"NTFS    " {
            return Err("not an NTFS volume".into());
        }
        let sector = u16at(&bs, 0x0B) as u64;
        let spc = bs[0x0D] as i8;
        let cluster = if spc > 0 {
            sector * spc as u64
        } else {
            1u64 << (-(spc as i32) as u32)
        };
        if !(256..=65536).contains(&sector) || cluster == 0 {
            return Err("boot sector geometry is not believable".into());
        }
        // Same signed encoding as sectors-per-cluster: negative means a power
        // of two in bytes rather than a count of clusters.
        let cpr = bs[0x40] as i8;
        let rec_size = if cpr > 0 {
            cluster * cpr as u64
        } else {
            1u64 << (-(cpr as i32) as u32)
        };
        if !(256..=65536).contains(&rec_size) {
            return Err("MFT record size is not believable".into());
        }

        let mft_off = base + u64at(&bs, 0x30) * cluster;
        let mut me = Ntfs {
            disk,
            base,
            cluster,
            rec_size,
            sector: sector as usize,
            mft: Vec::new(),
        };

        // Record 0 is the MFT's own entry, and its $DATA runs are the map of
        // the MFT itself. Everything else is read through it.
        let rec = me.read_record_at(mft_off)?;
        me.mft = attrs(&rec)
            .into_iter()
            .find(|(t, a)| *t == ATTR_DATA && a[A_NONRES] != 0)
            .map(|(_, a)| decode_runs(&a[u16at(a, A_RUN_OFF) as usize..]))
            .unwrap_or_default();
        if me.mft.is_empty() {
            // Fall back to treating the MFT as contiguous from where the boot
            // sector points. Wrong only for a fragmented MFT, and better than
            // refusing to look at all.
            me.mft = vec![((mft_off - base) / cluster, u64::MAX / cluster / 2)];
        }
        Ok(me)
    }

    fn read_record_at(&self, off: u64) -> Res<Vec<u8>> {
        let mut r = vec![0u8; self.rec_size as usize];
        self.disk.seek(off)?;
        self.disk.read(&mut r).ctx("read MFT record")?;
        Ok(r)
    }

    /// Byte offset of MFT record `n`, walking the MFT's own extents.
    fn record_offset(&self, n: u64) -> Option<u64> {
        let mut want = n.checked_mul(self.rec_size)?;
        for &(lcn, len) in &self.mft {
            let span = len.saturating_mul(self.cluster);
            if want < span {
                return Some(self.base + lcn * self.cluster + want);
            }
            want -= span;
        }
        None
    }

    pub fn records(&self) -> u64 {
        self.mft.iter().map(|&(_, l)| l * self.cluster).sum::<u64>() / self.rec_size
    }

    /// Deleted files with a name and a readable $DATA.
    pub fn deleted(&self, limit: usize, progress: impl Fn(u64, u64)) -> Vec<Deleted> {
        let total = self.records();
        let mut out = Vec::new();
        for n in 0..total {
            if n % 4096 == 0 {
                progress(n, total);
            }
            if out.len() >= limit {
                break;
            }
            let Some(off) = self.record_offset(n) else {
                break;
            };
            let Ok(mut rec) = self.read_record_at(off) else {
                continue;
            };
            if &rec[..4] != MAGIC || !apply_fixup(&mut rec, self.sector) {
                continue;
            }
            let flags = u16at(&rec, R_FLAGS);
            // bit 0 = in use, bit 1 = directory. Deleted files only.
            if flags & 1 != 0 || flags & 2 != 0 {
                continue;
            }

            let list = attrs(&rec);
            let Some(name) = list
                .iter()
                .filter(|(t, _)| *t == ATTR_FILE_NAME)
                .find_map(|(_, a)| file_name(a))
            else {
                continue;
            };
            let Some((_, data)) = list.iter().find(|(t, _)| *t == ATTR_DATA) else {
                continue;
            };

            let d = if data[A_NONRES] == 0 {
                let vo = u16at(data, A_RES_OFF) as usize;
                let vl = u32at(data, A_RES_LEN) as usize;
                match data.get(vo..vo + vl) {
                    Some(v) => Deleted {
                        name,
                        size: vl as u64,
                        resident: Some(v.to_vec()),
                        runs: vec![],
                    },
                    None => continue,
                }
            } else {
                let ro = u16at(data, A_RUN_OFF) as usize;
                let runs = data.get(ro..).map(decode_runs).unwrap_or_default();
                if runs.is_empty() {
                    continue;
                }
                Deleted {
                    name,
                    size: u64at(data, A_REAL_SIZE),
                    resident: None,
                    runs,
                }
            };
            out.push(d);
        }
        progress(total, total);
        out
    }

    /// Read a deleted file's content back off the volume.
    ///
    /// The clusters were marked free when it was deleted, so anything written
    /// since may be sitting in them. There is no way to tell from here -- what
    /// comes back is whatever is on the platter now.
    pub fn read_file(&self, d: &Deleted) -> Res<Vec<u8>> {
        if let Some(r) = &d.resident {
            return Ok(r.clone());
        }
        let mut out = Vec::with_capacity(d.size.min(64 << 20) as usize);
        for &(lcn, len) in &d.runs {
            let want = (len * self.cluster).min(d.size.saturating_sub(out.len() as u64));
            if want == 0 {
                break;
            }
            // Round up to whole sectors: a raw handle will not serve less.
            let read = want.div_ceil(self.cluster) * self.cluster;
            let mut buf = vec![0u8; read as usize];
            self.disk.seek(self.base + lcn * self.cluster)?;
            // The count matters. Ignoring it turns a failed read into a
            // correctly-sized file full of zeros, which looks like a
            // successful recovery and is the worst thing this could do.
            let got = self.disk.read(&mut buf).ctx("read file data")?;
            buf.truncate((got as u64).min(want) as usize);
            let short = buf.len() as u64 != want;
            out.extend_from_slice(&buf);
            if short {
                break;
            }
        }
        out.truncate(d.size as usize);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixup_restores_sector_tails() {
        let mut rec = vec![0u8; 1024];
        rec[..4].copy_from_slice(MAGIC);
        // update sequence array at 0x30: stamp then one entry per sector
        rec[R_USA_OFF..R_USA_OFF + 2].copy_from_slice(&0x30u16.to_le_bytes());
        rec[R_USA_CNT..R_USA_CNT + 2].copy_from_slice(&3u16.to_le_bytes());
        rec[0x30] = 0xAA;
        rec[0x31] = 0xBB;
        rec[0x32] = 0x11;
        rec[0x33] = 0x22; // real bytes of sector 0's tail
        rec[0x34] = 0x33;
        rec[0x35] = 0x44; // ... and sector 1's
        rec[510] = 0xAA;
        rec[511] = 0xBB;
        rec[1022] = 0xAA;
        rec[1023] = 0xBB;

        assert!(apply_fixup(&mut rec, 512));
        assert_eq!(&rec[510..512], &[0x11, 0x22]);
        assert_eq!(&rec[1022..1024], &[0x33, 0x44]);
    }

    #[test]
    fn fixup_rejects_a_torn_record() {
        let mut rec = vec![0u8; 1024];
        rec[R_USA_OFF..R_USA_OFF + 2].copy_from_slice(&0x30u16.to_le_bytes());
        rec[R_USA_CNT..R_USA_CNT + 2].copy_from_slice(&3u16.to_le_bytes());
        rec[0x30] = 0xAA;
        rec[0x31] = 0xBB;
        rec[510] = 0xAA;
        rec[511] = 0xBB;
        rec[1022] = 0x00;
        rec[1023] = 0x00; // second sector never got stamped
        assert!(
            !apply_fixup(&mut rec, 512),
            "mismatched stamp means a bad record"
        );
    }

    #[test]
    fn runs_are_relative_and_signed() {
        // 0x21 0x18 0x34 0x12 -> 1 length byte, 2 offset bytes: 0x18 clusters at 0x1234
        assert_eq!(
            decode_runs(&[0x21, 0x18, 0x34, 0x12, 0x00]),
            vec![(0x1234, 0x18)]
        );

        // second run's offset is a delta from the first, and may be negative
        let b = [0x21, 0x10, 0x00, 0x10, 0x21, 0x10, 0x00, 0xF0, 0x00];
        // 0x1000 then 0x1000 + (-0x1000) = 0
        assert_eq!(decode_runs(&b), vec![(0x1000, 0x10), (0x0000, 0x10)]);

        // sparse run: no offset field, position must not move
        let b = [0x11, 0x08, 0x20, 0x01, 0x08, 0x11, 0x08, 0x10, 0x00];
        let r = decode_runs(&b);
        assert_eq!(
            r,
            vec![(0x20, 0x08), (0x30, 0x08)],
            "a hole must not shift the LCN"
        );

        // truncated list stops cleanly rather than reading past the end
        assert_eq!(decode_runs(&[0x21, 0x18]), vec![]);
        assert_eq!(decode_runs(&[]), vec![]);
    }

    #[test]
    fn attrs_stop_at_the_end_marker() {
        let mut rec = vec![0u8; 512];
        rec[R_ATTRS..R_ATTRS + 2].copy_from_slice(&56u16.to_le_bytes());
        // one 24-byte attribute of type 0x30, then the end marker
        rec[56..60].copy_from_slice(&ATTR_FILE_NAME.to_le_bytes());
        rec[60..64].copy_from_slice(&24u32.to_le_bytes());
        rec[80..84].copy_from_slice(&ATTR_END.to_le_bytes());
        let a = attrs(&rec);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].0, ATTR_FILE_NAME);
        assert_eq!(a[0].1.len(), 24);
    }

    #[test]
    fn attrs_reject_a_nonsense_length() {
        let mut rec = vec![0u8; 512];
        rec[R_ATTRS..R_ATTRS + 2].copy_from_slice(&56u16.to_le_bytes());
        rec[56..60].copy_from_slice(&ATTR_DATA.to_le_bytes());
        rec[60..64].copy_from_slice(&9999u32.to_le_bytes()); // past the record
        assert!(attrs(&rec).is_empty());
    }
}
