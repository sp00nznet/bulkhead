//! The ATA SANITIZE feature set: telling a drive to erase itself.
//!
//! This is the real thing, as distinct from writing zeros over it. The drive
//! erases its own media, including the blocks it has quietly remapped out of
//! service over its life -- which no amount of overwriting from outside can
//! reach. On an SSD a crypto scramble also takes about a second, because it
//! throws away the key rather than touching the flash.
//!
//! SANITIZE is a separate feature set from ATA security, and unlike a security
//! erase it is not blocked by the frozen state that firmware leaves behind at
//! boot. That is why it is preferred wherever a drive offers it.
use std::ffi::c_void;
use std::mem::size_of;

use windows::Win32::Storage::IscsiDisc::{ATA_PASS_THROUGH_DIRECT, IOCTL_ATA_PASS_THROUGH_DIRECT};
use windows::Win32::System::IO::DeviceIoControl;

use crate::util::{Ctx, Res};
use crate::Raw;

const CMD_SANITIZE: u8 = 0xB4;

/// Feature values selecting which sanitize operation to run.
const FEAT_STATUS: u16 = 0x0000;
const FEAT_CRYPTO_SCRAMBLE: u16 = 0x0011;
const FEAT_BLOCK_ERASE: u16 = 0x0012;
const FEAT_OVERWRITE: u16 = 0x0014;

/// Each operation carries a key in the LBA field. Without the right one the
/// drive rejects the command -- a deliberate guard against issuing a wipe by
/// accident, and worth preserving rather than routing around.
const KEY_CRYPTO: u64 = 0x0000_43727970; // "Cryp"
const KEY_BLOCK: u64 = 0x0000_426B4572; // "BkEr"
/// Overwrite puts its pattern in the low 32 bits and this signature above it.
const KEY_OVERWRITE_SIG: u64 = 0x4F57 << 32; // "OW"

const ATA_FLAGS_DRDY_REQUIRED: u16 = 0x01;
const ATA_FLAGS_48BIT: u16 = 0x40;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    /// Discards the encryption key. Near-instant, and the strongest option on
    /// a self-encrypting drive.
    CryptoScramble,
    /// Erases every block, including remapped ones. Fast on flash.
    BlockErase,
    /// Writes a pattern over the whole medium, in the drive's own firmware.
    /// Slow, but the only sanitize many spinning disks offer.
    Overwrite,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "ata-sanitize-crypto" => Some(Kind::CryptoScramble),
            "ata-sanitize-block" => Some(Kind::BlockErase),
            "ata-sanitize-overwrite" => Some(Kind::Overwrite),
            _ => None,
        }
    }

    fn feature(self) -> u16 {
        match self {
            Kind::CryptoScramble => FEAT_CRYPTO_SCRAMBLE,
            Kind::BlockErase => FEAT_BLOCK_ERASE,
            Kind::Overwrite => FEAT_OVERWRITE,
        }
    }

    /// The LBA payload: a fixed key, except for overwrite which also carries
    /// the pattern to write.
    fn lba(self) -> u64 {
        match self {
            Kind::CryptoScramble => KEY_CRYPTO,
            Kind::BlockErase => KEY_BLOCK,
            // Zeros as the pattern, which is what a reader would expect to see.
            Kind::Overwrite => KEY_OVERWRITE_SIG,
        }
    }

    fn count(self) -> u16 {
        // Overwrite takes a pass count in the low four bits; one is plenty
        // when the drive is writing its own media.
        match self {
            Kind::Overwrite => 1,
            _ => 0,
        }
    }
}

/// Lay a 48-bit ATA command into the two task files.
///
/// The registers are split across "current" and "previous": the low byte of
/// each 16-bit register goes in current, the high byte in previous. Getting
/// that backwards sends a different command entirely, which for this command
/// set is worth being careful about.
pub fn task_files(feature: u16, count: u16, lba: u64, command: u8) -> ([u8; 8], [u8; 8]) {
    let mut cur = [0u8; 8];
    let mut prev = [0u8; 8];

    cur[0] = feature as u8;
    prev[0] = (feature >> 8) as u8;
    cur[1] = count as u8;
    prev[1] = (count >> 8) as u8;

    cur[2] = lba as u8; // LBA 7:0
    cur[3] = (lba >> 8) as u8; // 15:8
    cur[4] = (lba >> 16) as u8; // 23:16
    prev[2] = (lba >> 24) as u8; // 31:24
    prev[3] = (lba >> 32) as u8; // 39:32
    prev[4] = (lba >> 40) as u8; // 47:40

    cur[5] = 0x40; // device: LBA mode
    cur[6] = command;
    (cur, prev)
}

fn send(disk: &Raw, cur: [u8; 8], prev: [u8; 8], data: Option<&mut [u8]>, what: &str) -> Res<()> {
    let len = data.as_ref().map(|d| d.len()).unwrap_or(0);
    let mut apt = ATA_PASS_THROUGH_DIRECT {
        Length: size_of::<ATA_PASS_THROUGH_DIRECT>() as u16,
        AtaFlags: ATA_FLAGS_DRDY_REQUIRED
            | ATA_FLAGS_48BIT
            | if len > 0 { 0x02 } else { 0 }, // DATA_IN
        DataTransferLength: len as u32,
        // Generous: a block erase on a large drive can take minutes, and the
        // command is asynchronous anyway.
        TimeOutValue: 120,
        DataBuffer: data.map(|d| d.as_mut_ptr() as *mut c_void).unwrap_or(std::ptr::null_mut()),
        CurrentTaskFile: cur,
        PreviousTaskFile: prev,
        ..Default::default()
    };
    let mut ret = 0u32;
    unsafe {
        DeviceIoControl(
            disk.0, IOCTL_ATA_PASS_THROUGH_DIRECT,
            Some(&mut apt as *mut _ as *mut c_void),
            size_of::<ATA_PASS_THROUGH_DIRECT>() as u32,
            Some(&mut apt as *mut _ as *mut c_void),
            size_of::<ATA_PASS_THROUGH_DIRECT>() as u32,
            Some(&mut ret), None,
        ).ctx(what)?;
    }
    // The drive reports command failure in the status register rather than by
    // failing the ioctl, so a successful call is not a successful command.
    let status = apt.CurrentTaskFile[6];
    if status & 0x01 != 0 {
        return Err(format!("{what}: drive refused it (error register {:#04x})",
                           apt.CurrentTaskFile[0]).into());
    }
    Ok(())
}

/// Start a sanitize. Returns as soon as the drive accepts it; the work happens
/// afterwards and is followed with `status`.
pub fn start(disk: &Raw, kind: Kind) -> Res<()> {
    let (cur, prev) = task_files(kind.feature(), kind.count(), kind.lba(), CMD_SANITIZE);
    send(disk, cur, prev, None, "SANITIZE")
}

/// How far along the drive is: `(finished, percent)`.
///
/// Progress arrives as a fraction of 65536 in the count register, and 0xFFFF
/// means finished.
pub fn status(disk: &Raw) -> Res<(bool, u8)> {
    let (cur, prev) = task_files(FEAT_STATUS, 0, 0, CMD_SANITIZE);
    let mut apt = ATA_PASS_THROUGH_DIRECT {
        Length: size_of::<ATA_PASS_THROUGH_DIRECT>() as u16,
        AtaFlags: ATA_FLAGS_DRDY_REQUIRED | ATA_FLAGS_48BIT,
        TimeOutValue: 30,
        CurrentTaskFile: cur,
        PreviousTaskFile: prev,
        ..Default::default()
    };
    let mut ret = 0u32;
    unsafe {
        DeviceIoControl(
            disk.0, IOCTL_ATA_PASS_THROUGH_DIRECT,
            Some(&mut apt as *mut _ as *mut c_void),
            size_of::<ATA_PASS_THROUGH_DIRECT>() as u32,
            Some(&mut apt as *mut _ as *mut c_void),
            size_of::<ATA_PASS_THROUGH_DIRECT>() as u32,
            Some(&mut ret), None,
        ).ctx("SANITIZE STATUS")?;
    }
    let progress = u16::from_le_bytes([apt.CurrentTaskFile[1], apt.PreviousTaskFile[1]]);
    Ok(progress_of(progress))
}

/// Turn the raw progress word into a finished flag and a percentage.
pub fn progress_of(word: u16) -> (bool, u8) {
    if word == 0xFFFF {
        return (true, 100);
    }
    (false, ((word as u32 * 100) / 65536) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_split_low_and_high_bytes() {
        let (cur, prev) = task_files(0x0011, 0, KEY_CRYPTO, CMD_SANITIZE);
        assert_eq!(cur[0], 0x11, "feature low");
        assert_eq!(prev[0], 0x00, "feature high");
        assert_eq!(cur[6], CMD_SANITIZE);
        assert_eq!(cur[5], 0x40, "LBA mode must be set");
        // "Cryp" = 0x43727970, little end first across the LBA registers
        assert_eq!([cur[2], cur[3], cur[4], prev[2]], [0x70, 0x79, 0x72, 0x43]);
    }

    #[test]
    fn overwrite_carries_its_signature_above_the_pattern() {
        let (cur, prev) = task_files(FEAT_OVERWRITE, 1, Kind::Overwrite.lba(), CMD_SANITIZE);
        // pattern in the low 32 bits, zero here
        assert_eq!([cur[2], cur[3], cur[4], prev[2]], [0, 0, 0, 0]);
        // "OW" signature in bits 47:32
        assert_eq!([prev[3], prev[4]], [0x57, 0x4F]);
        assert_eq!(cur[1], 1, "one pass");
    }

    #[test]
    fn each_kind_carries_the_key_the_drive_demands() {
        // A wrong key is rejected by the drive, which is a guard against
        // issuing one of these by accident.
        assert_eq!(Kind::CryptoScramble.lba(), KEY_CRYPTO);
        assert_eq!(Kind::BlockErase.lba(), KEY_BLOCK);
        assert_ne!(Kind::CryptoScramble.lba(), Kind::BlockErase.lba());
        assert_eq!(Kind::CryptoScramble.feature(), 0x0011);
        assert_eq!(Kind::BlockErase.feature(), 0x0012);
        assert_eq!(Kind::Overwrite.feature(), 0x0014);
    }

    #[test]
    fn progress_reads_as_a_fraction_of_65536() {
        assert_eq!(progress_of(0xFFFF), (true, 100));
        assert_eq!(progress_of(0), (false, 0));
        assert_eq!(progress_of(32768), (false, 50));
        // Nearly there is not there: only the 0xFFFF sentinel means finished,
        // and rounding up to 100 must not be mistaken for it.
        let (done, pct) = progress_of(65534);
        assert!(!done);
        assert_eq!(pct, 99);
    }

    #[test]
    fn method_names_map_to_kinds() {
        assert_eq!(Kind::parse("ata-sanitize-crypto"), Some(Kind::CryptoScramble));
        assert_eq!(Kind::parse("ata-sanitize-block"), Some(Kind::BlockErase));
        assert_eq!(Kind::parse("overwrite"), None, "that is the software fallback");
    }
}
