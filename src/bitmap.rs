//! The volume's own cluster allocation bitmap.
//!
//! This is the difference between imaging a 500 GB disk and imaging the 80 GB
//! actually in use. The filesystem already knows which clusters hold data;
//! FSCTL_GET_VOLUME_BITMAP just asks it.
use std::ffi::c_void;
use std::mem::size_of;

use windows::core::HRESULT;
use windows::Win32::Foundation::{ERROR_MORE_DATA, HANDLE};
use windows::Win32::System::Ioctl::{FSCTL_GET_VOLUME_BITMAP, STARTING_LCN_INPUT_BUFFER};
use windows::Win32::System::IO::DeviceIoControl;

use crate::util::Res;

/// Header of VOLUME_BITMAP_BUFFER: StartingLcn then BitmapSize, both i64.
const HDR: usize = 16;
const OUT: usize = 1 << 20;

pub struct Bitmap {
    pub cluster: u64,
    pub clusters: u64,
    pub allocated: u64,
    bits: Vec<u8>,
}

impl Bitmap {
    /// Is any cluster overlapping the byte range `[start, end)` in use?
    ///
    /// Anything past the end of the bitmap counts as allocated: running off the
    /// map is a reason to copy, never a reason to skip.
    pub fn any_allocated(&self, start: u64, end: u64) -> bool {
        let first = start / self.cluster;
        let last = (end + self.cluster - 1) / self.cluster;
        (first..last).any(|c| {
            self.bits
                .get((c / 8) as usize)
                .map_or(true, |b| b & (1 << (c % 8)) != 0)
        })
    }
}

/// Read the whole allocation bitmap off an open volume handle.
///
/// Returns `None` when the filesystem does not offer one -- FAT via some
/// drivers, ReFS, anything unrecognised -- in which case the caller images
/// every sector, which is correct, just slower.
pub fn read(h: HANDLE, vol_size: u64) -> Res<Option<Bitmap>> {
    let mut out = vec![0u8; OUT];
    let mut bits: Vec<u8> = Vec::new();
    let mut lcn: i64 = 0;
    let mut total: i64 = 0;

    loop {
        let input = STARTING_LCN_INPUT_BUFFER { StartingLcn: lcn };
        let mut ret = 0u32;
        let r = unsafe {
            DeviceIoControl(
                h, FSCTL_GET_VOLUME_BITMAP,
                Some(&input as *const _ as *const c_void),
                size_of::<STARTING_LCN_INPUT_BUFFER>() as u32,
                Some(out.as_mut_ptr() as *mut c_void), OUT as u32,
                Some(&mut ret), None,
            )
        };
        // ERROR_MORE_DATA is the normal "here is a page of it" answer, not a
        // failure. Any other error means no bitmap is on offer.
        let more = match r {
            Ok(()) => false,
            Err(e) if e.code() == HRESULT::from_win32(ERROR_MORE_DATA.0) => true,
            Err(_) => return Ok(None),
        };
        if (ret as usize) < HDR {
            break;
        }

        let start = i64::from_le_bytes(out[0..8].try_into().unwrap());
        let size = i64::from_le_bytes(out[8..16].try_into().unwrap());
        if total == 0 {
            total = start + size;
        }
        let described = ((ret as usize - HDR) * 8).min(size as usize);
        if described == 0 {
            break;
        }
        bits.extend_from_slice(&out[HDR..HDR + described.div_ceil(8)]);

        lcn = start + described as i64;
        if !more || lcn >= total {
            break;
        }
    }

    if total <= 0 {
        return Ok(None);
    }
    // Derive the cluster size instead of parsing a BPB per filesystem: the
    // volume is `total` clusters long, and cluster sizes are powers of two.
    let cluster = (vol_size / total as u64).next_power_of_two();
    if !(512..=(2 << 20)).contains(&cluster) {
        return Ok(None);
    }

    let allocated = bits.iter().map(|b| b.count_ones() as u64).sum();
    Ok(Some(Bitmap { cluster, clusters: total as u64, allocated, bits }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(bits: Vec<u8>, cluster: u64, clusters: u64) -> Bitmap {
        Bitmap { cluster, clusters, allocated: 0, bits }
    }

    #[test]
    fn ranges() {
        // clusters 0 and 3 in use, 1/2/4..7 free, 4096-byte clusters
        let b = bm(vec![0b0000_1001], 4096, 8);
        assert!(b.any_allocated(0, 4096));
        assert!(!b.any_allocated(4096, 3 * 4096));
        assert!(b.any_allocated(3 * 4096, 4 * 4096));
        assert!(!b.any_allocated(4 * 4096, 8 * 4096));
        // a range straddling free and used clusters must be copied
        assert!(b.any_allocated(2 * 4096, 4 * 4096));
        // unaligned start, still lands on the used cluster 3
        assert!(b.any_allocated(3 * 4096 + 10, 3 * 4096 + 20));
        // past the end of the bitmap: assume in use, never skip
        assert!(b.any_allocated(64 * 4096, 65 * 4096));
    }
}
