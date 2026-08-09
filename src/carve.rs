//! Recover files from a disk with no usable filesystem left.
//!
//! When the MFT is gone there are no names, no sizes and no maps -- only bytes.
//! Most formats announce themselves with a magic number at the front, and many
//! mark their own end, so a file can be lifted out whole without knowing
//! anything about the filesystem that held it. This is what PhotoRec does.
//!
//! What it cannot do is reassemble a fragmented file: a carved file is one
//! contiguous stretch, so anything the filesystem scattered comes back broken
//! after the first fragment. Prefer `undelete` whenever the MFT survives.
use std::path::Path;

use crate::util::{human, Res};
use crate::Raw;

/// Files start where the filesystem put them, and filesystems allocate in
/// clusters. Checking one position in 512 rather than every byte is what makes
/// scanning a whole disk finish.
const STEP: u64 = 512;
const CHUNK: usize = 4 << 20;

pub struct Sig {
    pub magic: &'static [u8],
    /// Where the magic sits relative to the start of the file.
    pub at: usize,
    pub ext: &'static str,
    /// Marks the end of the file, when the format has one.
    pub footer: Option<&'static [u8]>,
    /// How much to take when there is no footer, and the cap when there is.
    pub max: u64,
}

pub const SIGS: &[Sig] = &[
    Sig { magic: &[0xFF, 0xD8, 0xFF], at: 0, ext: "jpg",
          footer: Some(&[0xFF, 0xD9]), max: 32 << 20 },
    Sig { magic: &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A], at: 0, ext: "png",
          footer: Some(b"IEND\xAE\x42\x60\x82"), max: 64 << 20 },
    Sig { magic: b"GIF89a", at: 0, ext: "gif", footer: Some(&[0x00, 0x3B]), max: 16 << 20 },
    Sig { magic: b"GIF87a", at: 0, ext: "gif", footer: Some(&[0x00, 0x3B]), max: 16 << 20 },
    Sig { magic: b"%PDF-", at: 0, ext: "pdf", footer: Some(b"%%EOF"), max: 128 << 20 },
    // Office and OpenDocument files are zip containers.
    Sig { magic: b"PK\x03\x04", at: 0, ext: "zip", footer: None, max: 64 << 20 },
    Sig { magic: b"ftyp", at: 4, ext: "mp4", footer: None, max: 512 << 20 },
    Sig { magic: b"SQLite format 3\0", at: 0, ext: "sqlite", footer: None, max: 256 << 20 },
    Sig { magic: b"7z\xBC\xAF\x27\x1C", at: 0, ext: "7z", footer: None, max: 512 << 20 },
    Sig { magic: b"Rar!\x1A\x07", at: 0, ext: "rar", footer: None, max: 512 << 20 },
    Sig { magic: b"ID3", at: 0, ext: "mp3", footer: None, max: 32 << 20 },
    Sig { magic: b"OggS", at: 0, ext: "ogg", footer: None, max: 128 << 20 },
    Sig { magic: b"\x1F\x8B\x08", at: 0, ext: "gz", footer: None, max: 256 << 20 },
    Sig { magic: b"BZh", at: 0, ext: "bz2", footer: None, max: 256 << 20 },
];

/// How many bytes of `data` belong to this file.
///
/// With a footer, everything up to and including it -- searched from past the
/// magic so a footer that happens to sit inside the header is not mistaken for
/// the end. Without one, all of it: the caller has already capped the length,
/// and trailing slack is better than a truncated file.
pub fn find_end(data: &[u8], sig: &Sig) -> usize {
    let Some(f) = sig.footer else { return data.len() };
    let from = sig.at + sig.magic.len();
    if data.len() < from + f.len() {
        return data.len();
    }
    for i in from..=data.len() - f.len() {
        if &data[i..i + f.len()] == f {
            return i + f.len();
        }
    }
    data.len()
}

pub fn carve(disk: &Raw, size: u64, out_dir: &Path, limit: usize) -> Res<usize> {
    std::fs::create_dir_all(out_dir)?;
    let longest = SIGS.iter().map(|s| s.at + s.magic.len()).max().unwrap_or(0);
    let mut buf = vec![0u8; CHUNK + 4096];
    let mut pos = 0u64;
    let mut found = 0usize;
    // Everything inside a file already carved out is that file's contents, not
    // a new one -- without this, a zip full of PNGs becomes a zip and a
    // hundred PNGs.
    let mut skip_to = 0u64;
    let mut last_pct = u64::MAX;

    while pos < size && found < limit {
        let want = ((size - pos) as usize).min(buf.len());
        disk.seek(pos)?;
        let n = disk.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }

        let mut off = 0usize;
        while off < n.min(CHUNK) && found < limit {
            let start = pos + off as u64;
            if start < skip_to {
                off += STEP as usize;
                continue;
            }
            for sig in SIGS {
                let a = off + sig.at;
                if a + sig.magic.len() > n || &buf[a..a + sig.magic.len()] != sig.magic {
                    continue;
                }
                let take = sig.max.min(size - start);
                let mut file = vec![0u8; take as usize];
                disk.seek(start)?;
                let got = disk.read(&mut file)?;
                file.truncate(got);
                let end = find_end(&file, sig);
                file.truncate(end);
                if file.len() < longest {
                    continue;
                }

                let name = out_dir.join(format!("{:06}_{}.{}", found, start / STEP, sig.ext));
                std::fs::write(&name, &file)?;
                eprintln!("\r  {} at {} ({})            ",
                          sig.ext, human(start), human(file.len() as u64));
                found += 1;
                skip_to = start + file.len() as u64;
                break;
            }
            off += STEP as usize;
        }

        pos += CHUNK as u64;
        let pct = pos.min(size) * 100 / size;
        if pct != last_pct {
            eprint!("\r  {pct:3}%  {} / {}", human(pos.min(size)), human(size));
            use std::io::Write;
            let _ = std::io::stderr().flush();
            last_pct = pct;
        }
    }
    eprintln!("\r  100%  {} / {}      ", human(size), human(size));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(ext: &str) -> &'static Sig {
        SIGS.iter().find(|s| s.ext == ext).unwrap()
    }

    #[test]
    fn footer_ends_the_file() {
        let jpg = sig("jpg");
        let mut d = vec![0xFF, 0xD8, 0xFF, 0xE0];
        d.extend_from_slice(&[0x41; 100]);
        d.extend_from_slice(&[0xFF, 0xD9]);
        d.extend_from_slice(&[0x00; 500]); // slack after the file
        assert_eq!(find_end(&d, jpg), 106);
    }

    #[test]
    fn footer_inside_the_header_is_not_the_end() {
        // A JPEG's own magic ends FF D8 FF; a naive search from zero could
        // match a footer overlapping it.
        let jpg = sig("jpg");
        let mut d = vec![0xFF, 0xD8, 0xFF, 0xD9];
        d.extend_from_slice(&[0x41; 50]);
        d.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(find_end(&d, jpg), d.len(), "must skip past the magic first");
    }

    #[test]
    fn missing_footer_takes_everything_offered() {
        let png = sig("png");
        let d = vec![0u8; 4096];
        assert_eq!(find_end(&d, png), 4096);
    }

    #[test]
    fn footerless_formats_take_the_whole_window() {
        let zip = sig("zip");
        assert!(zip.footer.is_none());
        assert_eq!(find_end(&[0u8; 1000], zip), 1000);
    }

    #[test]
    fn data_shorter_than_the_footer_does_not_panic() {
        assert_eq!(find_end(&[0xFF], sig("jpg")), 1);
        assert_eq!(find_end(&[], sig("jpg")), 0);
    }
}
