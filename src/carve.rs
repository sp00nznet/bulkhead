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

/// Reads a format's own length out of the front of one of its files, or returns
/// `None` if those bytes are not really that format.
type DeclaredLen = fn(&[u8]) -> Option<u64>;

pub struct Sig {
    pub magic: &'static [u8],
    /// Where the magic sits relative to the start of the file.
    pub at: usize,
    pub ext: &'static str,
    /// Marks the end of the file, when the format has one.
    pub footer: Option<&'static [u8]>,
    /// How much to take when there is no footer, and the cap when there is.
    pub max: u64,
    /// Formats that write their own length into their header. Better than a footer:
    /// the size is exact, so the file comes out byte for byte instead of padded to
    /// whatever `max` happens to be. Returning `None` rejects the candidate, which is
    /// how a four-byte magic that matched by chance gets thrown away.
    pub declared_len: Option<DeclaredLen>,
}

/// Enough of the front of a file to find a declared length in. Read before the file
/// itself, so it has to be small.
const HEAD: usize = 4096;

/// The length a PST or OST claims in its header, if this really is one.
///
/// Outlook mail stores are worth carving and awkward to carve: no footer, no fixed
/// size, and files that run to tens of gigabytes, so there is nothing to search for
/// and nothing safe to guess. The header settles it — `ibFileEof` is the exact length,
/// and the client signature and version are enough to throw out a chance `!BDN`.
fn pst_len(h: &[u8]) -> Option<u64> {
    if h.len() < 192 {
        return None;
    }
    // wMagicClient: "SM" for a PST, "SO" for an OST.
    let client = u16::from_le_bytes([h[8], h[9]]);
    if client != 0x4D53 && client != 0x4F53 {
        return None;
    }
    // wVer 23 is the Unicode PST, 36 and 37 the OST. 14 and 15 are the ANSI format,
    // whose ROOT sits at a different offset and is a different width -- refused rather
    // than read at the wrong offset and turned into a garbage length.
    if !matches!(u16::from_le_bytes([h[10], h[11]]), 23 | 36 | 37) {
        return None;
    }
    // ROOT starts at 180, and ibFileEof is the 8 bytes after its dwReserved.
    let len = u64::from_le_bytes(h[184..192].try_into().ok()?);
    // Smaller than its own header is not a file, and a wild number must never become
    // an allocation. 50GB is Outlook's own default ceiling for a mail store.
    (564..=50 << 30).contains(&len).then_some(len)
}

pub const SIGS: &[Sig] = &[
    Sig { magic: &[0xFF, 0xD8, 0xFF], at: 0, ext: "jpg",
          footer: Some(&[0xFF, 0xD9]), max: 32 << 20, declared_len: None },
    Sig { magic: &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A], at: 0, ext: "png",
          footer: Some(b"IEND\xAE\x42\x60\x82"), max: 64 << 20, declared_len: None },
    Sig { magic: b"GIF89a", at: 0, ext: "gif", footer: Some(&[0x00, 0x3B]), max: 16 << 20, declared_len: None },
    Sig { magic: b"GIF87a", at: 0, ext: "gif", footer: Some(&[0x00, 0x3B]), max: 16 << 20, declared_len: None },
    Sig { magic: b"%PDF-", at: 0, ext: "pdf", footer: Some(b"%%EOF"), max: 128 << 20, declared_len: None },
    // Office and OpenDocument files are zip containers.
    Sig { magic: b"PK\x03\x04", at: 0, ext: "zip", footer: None, max: 64 << 20, declared_len: None },
    Sig { magic: b"ftyp", at: 4, ext: "mp4", footer: None, max: 512 << 20, declared_len: None },
    Sig { magic: b"SQLite format 3\0", at: 0, ext: "sqlite", footer: None, max: 256 << 20, declared_len: None },
    Sig { magic: b"7z\xBC\xAF\x27\x1C", at: 0, ext: "7z", footer: None, max: 512 << 20, declared_len: None },
    Sig { magic: b"Rar!\x1A\x07", at: 0, ext: "rar", footer: None, max: 512 << 20, declared_len: None },
    Sig { magic: b"ID3", at: 0, ext: "mp3", footer: None, max: 32 << 20, declared_len: None },
    Sig { magic: b"OggS", at: 0, ext: "ogg", footer: None, max: 128 << 20, declared_len: None },
    Sig { magic: b"\x1F\x8B\x08", at: 0, ext: "gz", footer: None, max: 256 << 20, declared_len: None },
    Sig { magic: b"BZh", at: 0, ext: "bz2", footer: None, max: 256 << 20, declared_len: None },
    // Outlook mail. Often the single file on a disk that someone actually wants back,
    // and the one no footer can find the end of. `max` here is not the format's limit
    // but this carver's: a candidate is buffered whole, so it is what fits in memory.
    // ponytail: 2GB, and it prints when it has to cut. Streaming the write would lift
    // the ceiling to the 50GB the format allows, and is the fix if that ever bites.
    Sig { magic: b"!BDN", at: 0, ext: "pst", footer: None, max: 2 << 30,
          declared_len: Some(pst_len) },
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
                // A format that states its own length is read in two goes: enough of
                // the front to find the number, then exactly the bytes it names. The
                // alternative is allocating `max` for every candidate, which is fine
                // for a 32MB photo and impossible for a mail store.
                let take = match sig.declared_len {
                    Some(len_of) => {
                        let mut head = vec![0u8; HEAD.min((size - start) as usize)];
                        disk.seek(start)?;
                        let got = disk.read(&mut head)?;
                        head.truncate(got);
                        let Some(len) = len_of(&head) else { continue };
                        let want = len.min(size - start);
                        if want > sig.max {
                            eprintln!(
                                "\r  {} at {} declares {} — taking the first {}, the rest needs a streaming write",
                                sig.ext, human(start), human(want), human(sig.max)
                            );
                        }
                        want.min(sig.max)
                    }
                    None => sig.max.min(size - start),
                };
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

    /// A header good enough to be believed: `!BDN`, a client signature, a version and
    /// a declared length. Everything between is what a real one would have and this
    /// does not need.
    fn pst_header(client: &[u8; 2], ver: u16, len: u64) -> Vec<u8> {
        let mut h = vec![0u8; 564];
        h[..4].copy_from_slice(b"!BDN");
        h[8..10].copy_from_slice(client);
        h[10..12].copy_from_slice(&ver.to_le_bytes());
        h[184..192].copy_from_slice(&len.to_le_bytes());
        h
    }

    #[test]
    fn a_pst_states_its_own_length() {
        let pst = sig("pst");
        let len_of = pst.declared_len.expect("the pst signature reads its header");
        // A PST and an OST, both of which say how long they are.
        assert_eq!(len_of(&pst_header(b"SM", 23, 271_360)), Some(271_360));
        assert_eq!(len_of(&pst_header(b"SO", 36, 16_818_176)), Some(16_818_176));
    }

    #[test]
    fn a_chance_bdn_is_rejected() {
        let len_of = sig("pst").declared_len.unwrap();
        // Four bytes of anything can spell !BDN. Everything that is not a real header
        // has to come back None, or the carver writes gigabytes of someone else's data.
        assert_eq!(len_of(&pst_header(b"XX", 23, 4096)), None, "wrong client signature");
        assert_eq!(len_of(&pst_header(b"SM", 99, 4096)), None, "unknown format version");
        assert_eq!(len_of(&pst_header(b"SM", 23, 8)), None, "shorter than its own header");
        assert_eq!(len_of(&pst_header(b"SM", 23, u64::MAX)), None, "absurd length");
        assert_eq!(len_of(b"!BDN"), None, "nothing but the magic");
        // ANSI PSTs keep ROOT somewhere else, so reading 184 would invent a length.
        assert_eq!(len_of(&pst_header(b"SM", 14, 4096)), None, "ANSI is refused, not guessed");
    }

    #[test]
    fn a_declared_length_beats_the_slack_a_footerless_format_gets() {
        // The point of the whole exercise: 271360 bytes of PST inside a much bigger
        // window comes back as exactly 271360, where a footerless format would take
        // everything on offer.
        let pst = sig("pst");
        let want = 271_360u64;
        let mut d = pst_header(b"SM", 23, want);
        d.resize(4 << 20, 0x41);
        assert_eq!(pst.declared_len.unwrap()(&d), Some(want));
        assert!(pst.footer.is_none(), "a PST has no end marker to find");
        assert_eq!(find_end(&d, pst), d.len(), "so find_end cannot be what sizes it");
    }

    /// The whole path, on a disk image with a PST buried in it.
    ///
    /// The unit tests above only ask whether the header can be read. This asks the
    /// question that matters: does a PST sitting in the middle of a lot of other bytes
    /// come back out as itself, at its own length, rather than padded to whatever the
    /// carver felt like taking.
    #[test]
    fn carves_a_pst_out_of_an_image_at_exactly_its_own_length() {
        let dir = std::env::temp_dir().join("bulkhead-carve-pst-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let len = 271_360usize;
        let mut pst = pst_header(b"SM", 23, len as u64);
        pst.resize(len, 0xAB);

        // Junk, the PST on a sector boundary the way a filesystem would place it, junk.
        let mut img = vec![0x5Au8; 512 * 9];
        img.extend_from_slice(&pst);
        img.extend(std::iter::repeat(0x5A).take(64 << 10));
        let img_path = dir.join("image.bin");
        std::fs::write(&img_path, &img).unwrap();

        let out = dir.join("out");
        let disk = crate::Raw::open(img_path.to_str().unwrap(), false).unwrap();
        carve(&disk, img.len() as u64, &out, 10).unwrap();

        let carved: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pst"))
            .collect();
        assert_eq!(carved.len(), 1, "expected exactly one PST out of the image");
        let got = std::fs::read(carved[0].path()).unwrap();
        assert_eq!(got.len(), len, "the declared length has to size the carve exactly");
        assert_eq!(got, pst, "and the bytes have to be the file that went in");
        let _ = std::fs::remove_dir_all(&dir);
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
