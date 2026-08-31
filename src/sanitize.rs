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

use windows::Win32::Storage::IscsiDisc::{
    IOCTL_SCSI_PASS_THROUGH_DIRECT, SCSI_PASS_THROUGH_DIRECT,
};
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

/// The SCSI opcode that carries an ATA command inside a SCSI CDB.
const SCSI_ATA_PASS_THROUGH_16: u8 = 0x85;
/// Protocol 3 is a non-data command -- every one of these is.
const PROTOCOL_NON_DATA: u8 = 3;
/// Ask the SAT layer to return the drive's registers in the sense data. Without
/// it a refusal is invisible: the call succeeds and the drive did nothing.
const CK_COND: u8 = 0x20;

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

/// Lay a 48-bit ATA command into an ATA PASS-THROUGH(16) CDB.
///
/// Each 16-bit register is split across two CDB bytes, and the halves are
/// interleaved rather than adjacent: byte 7 is LBA(31:24) but byte 8 is
/// LBA(7:0). Getting that wrong sends a different command entirely, which for
/// this command set is worth being careful about.
pub fn cdb16(feature: u16, count: u16, lba: u64, command: u8) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0] = SCSI_ATA_PASS_THROUGH_16;
    c[1] = (PROTOCOL_NON_DATA << 1) | 1; // EXTEND: this is a 48-bit command
    c[2] = CK_COND; // hand the drive's registers back in the sense data
    c[3] = (feature >> 8) as u8;
    c[4] = feature as u8;
    c[5] = (count >> 8) as u8;
    c[6] = count as u8;
    c[7] = (lba >> 24) as u8;
    c[8] = lba as u8;
    c[9] = (lba >> 32) as u8;
    c[10] = (lba >> 8) as u8;
    c[11] = (lba >> 40) as u8;
    c[12] = (lba >> 16) as u8;
    c[13] = 0x40; // device: LBA mode
    c[14] = command;
    c
}

/// The drive's task-file registers, as they come back from a command.
#[derive(Debug, Default, Clone, Copy)]
pub struct Regs {
    pub status: u8,
    pub error: u8,
    pub count: u16,
    pub lba: u64,
}

impl Regs {
    /// The drive reports a refusal in its own registers, not by failing the
    /// call, so a command that went through is not a command that was obeyed.
    fn refused(&self) -> bool {
        self.status & 0x01 != 0
    }
}

/// Pull the ATA Status Return descriptor out of descriptor-format sense.
///
/// `CK_COND` asks the SAT layer for the registers; they come back as one
/// descriptor among possibly several, so walk the list rather than assuming
/// position.
fn ata_regs(sense: &[u8]) -> Option<Regs> {
    // 0x72 current, 0x73 deferred. The fixed-format codes carry no descriptors.
    if sense.len() < 8 || (sense[0] != 0x72 && sense[0] != 0x73) {
        return None;
    }
    let end = (8 + sense[7] as usize).min(sense.len());
    let mut i = 8;
    while i + 2 <= end {
        let len = sense[i + 1] as usize;
        if sense[i] == 0x09 && i + 14 <= end {
            let d = &sense[i..i + 14];
            return Some(Regs {
                error: d[3],
                count: u16::from_be_bytes([d[4], d[5]]),
                // Same interleave as the CDB, unpicked.
                lba: u64::from_le_bytes([d[7], d[9], d[11], d[6], d[8], d[10], 0, 0]),
                status: d[13],
            });
        }
        i += 2 + len;
    }
    None
}

#[repr(C)]
struct ScsiReq {
    spt: SCSI_PASS_THROUGH_DIRECT,
    sense: [u8; 32],
}

/// Send one ATA command, tunnelled through SCSI, and read the registers back.
///
/// Deliberately **not** `IOCTL_ATA_PASS_THROUGH_DIRECT`. Windows' `storahci`
/// refuses SANITIZE (opcode 0xB4) on that path with ERROR_NOT_SUPPORTED,
/// before the command ever reaches the drive -- IDENTIFY and READ VERIFY EXT
/// go through the very same call untouched, so it is the opcode being
/// filtered, not the request. Wrapping the identical command in a SCSI CDB and
/// letting the driver's SAT layer unwrap it works on every drive tried, and
/// the drive's own verdict comes back in the sense data. `examples/atprobe.rs`
/// is the experiment that established this; keep it, it is the only thing that
/// tells you which layer is refusing.
fn send(disk: &Raw, cdb: [u8; 16], what: &str) -> Res<Option<Regs>> {
    let mut req = ScsiReq {
        spt: SCSI_PASS_THROUGH_DIRECT {
            Length: size_of::<SCSI_PASS_THROUGH_DIRECT>() as u16,
            CdbLength: 16,
            SenseInfoLength: 32,
            DataIn: 2, // SCSI_IOCTL_DATA_UNSPECIFIED: none of these carry data
            DataTransferLength: 0,
            // Generous: a block erase on a large drive can take minutes, and
            // the command is asynchronous anyway.
            TimeOutValue: 120,
            DataBuffer: std::ptr::null_mut(),
            SenseInfoOffset: size_of::<SCSI_PASS_THROUGH_DIRECT>() as u32,
            Cdb: cdb,
            ..Default::default()
        },
        sense: [0u8; 32],
    };
    let mut ret = 0u32;
    let sz = size_of::<ScsiReq>() as u32;
    unsafe {
        DeviceIoControl(
            disk.0, IOCTL_SCSI_PASS_THROUGH_DIRECT,
            Some(&mut req as *mut _ as *mut c_void), sz,
            Some(&mut req as *mut _ as *mut c_void), sz,
            Some(&mut ret), None,
        ).ctx(what)?;
    }
    let regs = ata_regs(&req.sense);
    if let Some(r) = regs {
        if r.refused() {
            return Err(format!("{what}: drive refused it (error register {:#04x})",
                               r.error).into());
        }
    }
    // No descriptor is not evidence of refusal, and reporting a failure that
    // did not happen is the worse error here: it would say a sanitize failed
    // while the drive is busy erasing itself. `status` is the source of truth.
    Ok(regs)
}

/// Start a sanitize. Returns as soon as the drive accepts it; the work happens
/// afterwards and is followed with `status`.
pub fn start(disk: &Raw, kind: Kind) -> Res<()> {
    send(disk, cdb16(kind.feature(), kind.count(), kind.lba(), CMD_SANITIZE), "SANITIZE")?;
    Ok(())
}

/// How far along the drive is: `(finished, percent)`.
pub fn status(disk: &Raw) -> Res<(bool, u8)> {
    let regs = send(disk, cdb16(FEAT_STATUS, 0, 0, CMD_SANITIZE), "SANITIZE STATUS")?
        .ok_or("SANITIZE STATUS: the drive returned no registers")?;
    Ok(progress_of(progress_word(&regs)))
}

/// Which register carries the progress word.
///
/// This has never been watched on a drive mid-sanitize, so the honest answer
/// is that it is not yet known. What *has* been seen, on two drives from
/// different vendors with nothing running, is count=0x0000 alongside the
/// 0xFFFF "not in progress" sentinel in LBA(15:0) -- so reading only the count
/// register, as this did before, would have sat at 0% forever and never seen
/// the drive finish.
///
/// ponytail: read both and let either sentinel mean done, which is right
/// whichever register the drive actually uses. Pin it to the one register once
/// a real sanitize has been watched from start to finish.
fn progress_word(r: &Regs) -> u16 {
    let lba = r.lba as u16;
    if r.count == 0xFFFF || lba == 0xFFFF {
        return 0xFFFF;
    }
    // Whichever one is counting; the other reads zero.
    r.count.max(lba)
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
        let c = cdb16(0x0011, 0, KEY_CRYPTO, CMD_SANITIZE);
        assert_eq!(c[0], SCSI_ATA_PASS_THROUGH_16);
        assert_eq!(c[1] & 0x01, 1, "EXTEND: without it the high half is dropped");
        assert_eq!((c[1] >> 1) & 0x0F, PROTOCOL_NON_DATA);
        assert_eq!(c[2], CK_COND, "without it a refusal is invisible");
        assert_eq!([c[3], c[4]], [0x00, 0x11], "feature high then low");
        assert_eq!(c[14], CMD_SANITIZE);
        assert_eq!(c[13], 0x40, "LBA mode must be set");
        // "Cryp" = 0x43727970, interleaved across the CDB's LBA bytes:
        // 7:0, 15:8, 23:16, 31:24
        assert_eq!([c[8], c[10], c[12], c[7]], [0x70, 0x79, 0x72, 0x43]);
    }

    #[test]
    fn overwrite_carries_its_signature_above_the_pattern() {
        let c = cdb16(FEAT_OVERWRITE, 1, Kind::Overwrite.lba(), CMD_SANITIZE);
        // pattern in the low 32 bits, zero here
        assert_eq!([c[8], c[10], c[12], c[7]], [0, 0, 0, 0]);
        // "OW" signature in bits 47:32
        assert_eq!([c[9], c[11]], [0x57, 0x4F]);
        assert_eq!([c[5], c[6]], [0, 1], "one pass");
    }

    // The two cases below are real sense buffers captured from real drives on
    // 2026-08-31, not hand-written ones -- which is the point of keeping them.
    #[test]
    fn reads_the_registers_a_sanitize_capable_drive_returns() {
        // WDC WUH721818ALE6L4, idle: no error, and the 0xFFFF "not in
        // progress" sentinel in LBA rather than in count.
        let sense = [0x72, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0E,
                     0x09, 0x0C, 0x01, 0x00, 0x00, 0x00, 0x00, 0xFF,
                     0x00, 0xFF, 0x00, 0x00, 0x00, 0x50];
        let r = ata_regs(&sense).expect("ATA status descriptor");
        assert_eq!(r.status, 0x50);
        assert_eq!(r.error, 0x00);
        assert!(!r.refused());
        assert_eq!(r.count, 0x0000);
        assert_eq!(r.lba as u16, 0xFFFF);
        // Reading count alone would have called this 0% and never finished.
        assert_eq!(progress_of(progress_word(&r)), (true, 100));
    }

    #[test]
    fn a_drive_without_sanitize_comes_back_aborted() {
        // Samsung PM851, which advertises no sanitize: ERR set, ABRT in the
        // error register. The command reached the drive and the drive said no.
        let sense = [0x72, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0E,
                     0x09, 0x0C, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00,
                     0x00, 0x00, 0x00, 0x00, 0xE0, 0x51];
        let r = ata_regs(&sense).expect("ATA status descriptor");
        assert_eq!(r.error & 0x04, 0x04, "ABRT");
        assert!(r.refused(), "a refusal must not read as success");
    }

    #[test]
    fn sense_without_an_ata_descriptor_is_not_a_refusal() {
        // All-zero sense is what a command with no CK_COND returns. Treating
        // it as failure would report a running sanitize as a failed one.
        assert!(ata_regs(&[0u8; 32]).is_none());
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
