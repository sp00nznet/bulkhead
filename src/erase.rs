//! Ask a drive to erase itself, and say what it can do before you try.
//!
//! Blancco and KillDisk charge per drive for what is, underneath, one command
//! the drive already implements plus a piece of paper. The command is the easy
//! part. The parts worth building are knowing *which* command a given drive
//! will accept, and producing a record afterwards that means something.
//!
//! Nothing here writes to a disk except `sanitize`, which is behind its own
//! confirmation and is the only destructive path in this file.
use std::ffi::c_void;
use std::mem::size_of;

use windows::Win32::Storage::IscsiDisc::{ATA_PASS_THROUGH_DIRECT, IOCTL_ATA_PASS_THROUGH_DIRECT};
use windows::Win32::System::Ioctl::{
    STORAGE_PROPERTY_QUERY, STORAGE_PROTOCOL_COMMAND, STORAGE_PROTOCOL_TYPE,
    IOCTL_STORAGE_PROTOCOL_COMMAND, IOCTL_STORAGE_QUERY_PROPERTY,
};
use windows::Win32::System::IO::DeviceIoControl;

use crate::util::{Ctx, Res};
use crate::Raw;

/// How the drive is attached. Which erase commands exist depends on it, and a
/// USB bridge usually hides all of them.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Bus {
    Ata,
    Nvme,
    Usb,
    Other(u8),
}

impl Bus {
    fn name(self) -> String {
        match self {
            Bus::Ata => "SATA/ATA".into(),
            Bus::Nvme => "NVMe".into(),
            Bus::Usb => "USB".into(),
            Bus::Other(n) => format!("bus type {n:#x}"),
        }
    }
}

/// What a drive says about erasing itself.
#[derive(Debug, Default)]
pub struct Caps {
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub bus: Option<Bus>,
    /// ATA: the drive implements the security feature set.
    pub ata_security: bool,
    /// ATA: SECURITY ERASE UNIT is blocked until a power cycle, because the
    /// firmware froze the security state at boot. Almost every desktop does
    /// this, and it is the usual reason an erase will not start.
    pub ata_frozen: bool,
    pub ata_security_enabled: bool,
    /// ATA: minutes the drive estimates for a normal and an enhanced erase.
    pub ata_erase_minutes: Option<(u16, u16)>,
    pub ata_enhanced_erase: bool,
    /// ATA: the SANITIZE feature set, which is not blocked by a frozen state.
    pub ata_sanitize: bool,
    pub ata_sanitize_crypto: bool,
    pub ata_sanitize_block: bool,
    pub ata_sanitize_overwrite: bool,
    /// NVMe: Format NVM is supported, and whether it can do a crypto erase.
    pub nvme_format: bool,
    pub nvme_crypto_erase: bool,
    /// NVMe: the Sanitize command, and which kinds.
    pub nvme_sanitize_crypto: bool,
    pub nvme_sanitize_block: bool,
    pub nvme_sanitize_overwrite: bool,
}

impl Caps {
    /// The commands that could actually be issued, best first.
    pub fn methods(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.nvme_sanitize_crypto {
            v.push("nvme-sanitize-crypto");
        }
        if self.nvme_sanitize_block {
            v.push("nvme-sanitize-block");
        }
        if self.nvme_crypto_erase {
            v.push("nvme-format-crypto");
        }
        if self.nvme_format {
            v.push("nvme-format");
        }
        if self.ata_sanitize_crypto {
            v.push("ata-sanitize-crypto");
        }
        if self.ata_sanitize_block {
            v.push("ata-sanitize-block");
        }
        // Slow -- it writes the whole surface -- but a real erase, and the only
        // one many enterprise drives offer.
        if self.ata_sanitize_overwrite {
            v.push("ata-sanitize-overwrite");
        }
        if self.ata_security && !self.ata_frozen {
            v.push("ata-security-erase");
        }
        v
    }

    /// Why an erase would not start right now, if it would not.
    pub fn blockers(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.bus == Some(Bus::Usb) {
            v.push("attached over USB: bridges rarely pass these commands through, \
                    and a bridge that half-implements them can report success \
                    without erasing anything. Connect it directly."
                .into());
        }
        if self.ata_security && self.ata_frozen && !self.ata_sanitize {
            v.push("security state is FROZEN: the firmware locks it at boot. \
                    Suspend and resume the machine, or hot-plug the drive, then \
                    check again."
                .into());
        }
        if self.ata_security_enabled {
            v.push("an ATA password is already set; it must be known to erase".into());
        }
        if self.methods().is_empty() {
            v.push("this drive reports no usable erase command".into());
        }
        v
    }
}

fn u16le(b: &[u8], word: usize) -> u16 {
    u16::from_le_bytes([b[word * 2], b[word * 2 + 1]])
}

/// ATA strings are byte-swapped within each 16-bit word.
fn ata_string(b: &[u8], first: usize, words: usize) -> String {
    let mut s = String::new();
    for w in first..first + words {
        s.push(b[w * 2 + 1] as char);
        s.push(b[w * 2] as char);
    }
    s.trim().to_string()
}

/// StorageDeviceProperty: model, serial and how the drive is attached.
fn device_property(disk: &Raw, caps: &mut Caps) -> Res<()> {
    let mut query = STORAGE_PROPERTY_QUERY::default();
    query.PropertyId = windows::Win32::System::Ioctl::StorageDeviceProperty;
    query.QueryType = windows::Win32::System::Ioctl::PropertyStandardQuery;
    let mut buf = vec![0u8; 4096];
    let mut ret = 0u32;
    unsafe {
        DeviceIoControl(
            disk.0, IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const c_void),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(buf.as_mut_ptr() as *mut c_void), buf.len() as u32,
            Some(&mut ret), None,
        ).ctx("IOCTL_STORAGE_QUERY_PROPERTY")?;
    }
    // STORAGE_DEVICE_DESCRIPTOR: offsets into the same buffer, or 0 if absent.
    let at = |off: usize| -> String {
        if off + 4 > buf.len() {
            return String::new();
        }
        let o = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        if o == 0 || o >= buf.len() {
            return String::new();
        }
        let end = buf[o..].iter().position(|&c| c == 0).unwrap_or(0) + o;
        String::from_utf8_lossy(&buf[o..end]).trim().to_string()
    };
    // STORAGE_DEVICE_DESCRIPTOR: Version, Size, DeviceType, DeviceTypeModifier,
    // RemovableMedia, CommandQueueing, then four offsets and the bus type.
    let vendor = at(12);
    let product = at(16);
    caps.firmware = at(20); // ProductRevision
    caps.serial = at(24);
    caps.model = format!("{vendor} {product}").trim().to_string();
    caps.bus = Some(match buf.get(28).copied().unwrap_or(0) {
        0x0B => Bus::Ata,
        0x11 => Bus::Nvme,
        0x07 => Bus::Usb,
        n => Bus::Other(n),
    });
    Ok(())
}

/// ATA IDENTIFY DEVICE, which is where every ATA capability bit lives.
fn ata_identify(disk: &Raw, caps: &mut Caps) -> Res<()> {
    let mut data = vec![0u8; 512];
    let mut apt = ATA_PASS_THROUGH_DIRECT {
        Length: size_of::<ATA_PASS_THROUGH_DIRECT>() as u16,
        // DRDY_REQUIRED | DATA_IN: the drive must be ready, and this command
        // returns data.
        AtaFlags: 0x01 | 0x02,
        DataTransferLength: 512,
        TimeOutValue: 10,
        DataBuffer: data.as_mut_ptr() as *mut c_void,
        ..Default::default()
    };
    apt.CurrentTaskFile[6] = 0xEC; // IDENTIFY DEVICE
    let mut ret = 0u32;
    unsafe {
        DeviceIoControl(
            disk.0, IOCTL_ATA_PASS_THROUGH_DIRECT,
            Some(&mut apt as *mut _ as *mut c_void),
            size_of::<ATA_PASS_THROUGH_DIRECT>() as u32,
            Some(&mut apt as *mut _ as *mut c_void),
            size_of::<ATA_PASS_THROUGH_DIRECT>() as u32,
            Some(&mut ret), None,
        ).ctx("ATA IDENTIFY")?;
    }
    if data.iter().all(|&b| b == 0) {
        return Err("IDENTIFY returned nothing".into());
    }

    if caps.model.is_empty() {
        caps.model = ata_string(&data, 27, 20);
        caps.serial = ata_string(&data, 10, 10);
        caps.firmware = ata_string(&data, 23, 4);
    }

    // Word 82 bit 1: security feature set supported.
    caps.ata_security = u16le(&data, 82) & 0x0002 != 0;
    // Word 128: security status.
    let sec = u16le(&data, 128);
    caps.ata_security_enabled = sec & 0x0002 != 0;
    caps.ata_frozen = sec & 0x0008 != 0;
    caps.ata_enhanced_erase = sec & 0x0020 != 0;
    // Words 89/90: erase time in units of 2 minutes, capped at 508.
    let t = |w: u16| -> u16 { (w & 0x00FF).saturating_mul(2) };
    if caps.ata_security {
        caps.ata_erase_minutes = Some((t(u16le(&data, 89)), t(u16le(&data, 90))));
    }
    // Word 59: sanitize support and which kinds.
    let w59 = u16le(&data, 59);
    caps.ata_sanitize = w59 & 0x1000 != 0;
    caps.ata_sanitize_crypto = w59 & 0x2000 != 0;
    caps.ata_sanitize_block = w59 & 0x8000 != 0;
    caps.ata_sanitize_overwrite = w59 & 0x4000 != 0;
    Ok(())
}

/// NVMe Identify Controller, for the format and sanitize capability words.
fn nvme_identify(disk: &Raw, caps: &mut Caps) -> Res<()> {
    // The command and its returned data share one buffer, placed by offsets.
    // Those offsets are measured from the start of the header to its Command
    // field -- not from the struct's size, which includes tail padding. Four
    // bytes of difference is enough for the driver to reject the whole thing
    // with ERROR_INVALID_PARAMETER.
    const CMD: usize = 64; // NVMe commands are 64 bytes
    const DATA: usize = 4096;
    let head = std::mem::offset_of!(STORAGE_PROTOCOL_COMMAND, Command);
    let mut buf = vec![0u8; head + CMD + DATA];

    {
        let c = buf.as_mut_ptr() as *mut STORAGE_PROTOCOL_COMMAND;
        unsafe {
            (*c).Version = head as u32;
            (*c).Length = head as u32;
            (*c).ProtocolType = STORAGE_PROTOCOL_TYPE(3); // ProtocolTypeNvme
            (*c).Flags = 0x8000_0000; // ADAPTER_REQUEST
            (*c).CommandLength = CMD as u32;
            (*c).DataFromDeviceTransferLength = DATA as u32;
            (*c).DataFromDeviceBufferOffset = (head + CMD) as u32;
            (*c).TimeOutValue = 10;
        }
    }
    // NVMe Identify: opcode 0x06, CDW10 = 1 (Identify Controller).
    buf[head] = 0x06;
    buf[head + 40..head + 44].copy_from_slice(&1u32.to_le_bytes());

    let mut ret = 0u32;
    let r = unsafe {
        DeviceIoControl(
            disk.0, IOCTL_STORAGE_PROTOCOL_COMMAND,
            Some(buf.as_mut_ptr() as *mut c_void), buf.len() as u32,
            Some(buf.as_mut_ptr() as *mut c_void), buf.len() as u32,
            Some(&mut ret), None,
        )
    };
    r.ctx("NVMe Identify Controller")?;

    let id = &buf[head + CMD..];
    if id.iter().all(|&b| b == 0) {
        return Err("Identify Controller returned nothing".into());
    }
    if caps.model.is_empty() {
        caps.serial = String::from_utf8_lossy(&id[4..24]).trim().to_string();
        caps.model = String::from_utf8_lossy(&id[24..64]).trim().to_string();
        caps.firmware = String::from_utf8_lossy(&id[64..72]).trim().to_string();
    }
    // OACS (bytes 256..258) bit 1: Format NVM supported.
    let oacs = u16::from_le_bytes([id[256], id[257]]);
    caps.nvme_format = oacs & 0x0002 != 0;
    // FNA (byte 524) bit 2: cryptographic erase supported by Format.
    caps.nvme_crypto_erase = caps.nvme_format && id[524] & 0x04 != 0;
    // SANICAP (bytes 328..332): which sanitize operations exist.
    let sanicap = u32::from_le_bytes(id[328..332].try_into().unwrap());
    caps.nvme_sanitize_crypto = sanicap & 0x0000_0004 != 0;
    caps.nvme_sanitize_block = sanicap & 0x0000_0002 != 0;
    caps.nvme_sanitize_overwrite = sanicap & 0x0000_0001 != 0;
    Ok(())
}

/// Everything the drive will tell us about erasing itself, and why any probe
/// did not answer.
///
/// The failures matter as much as the results: a drive reporting no erase
/// command usually means the query did not reach it, not that a modern SSD
/// cannot erase itself. Saying which probe failed and how is the difference
/// between a wrong answer and a diagnosable one.
pub fn capabilities(disk: &Raw) -> (Caps, Vec<String>) {
    let mut caps = Caps::default();
    let mut notes = Vec::new();
    let mut note = |what: &str, r: Res<()>| {
        if let Err(e) = r {
            notes.push(format!("{what}: {e}"));
        }
    };
    note("device properties", device_property(disk, &mut caps));
    // Each of these is expected to fail on the wrong kind of drive: an NVMe
    // device has no ATA IDENTIFY, and vice versa.
    let is_nvme = caps.bus == Some(Bus::Nvme);
    if !is_nvme {
        note("ATA IDENTIFY", ata_identify(disk, &mut caps));
    }
    if caps.bus.is_none() || is_nvme {
        note("NVMe Identify Controller", nvme_identify(disk, &mut caps));
    }
    (caps, notes)
}

pub fn report(caps: &Caps) -> Vec<String> {
    let mut v = vec![
        format!("model    {}", caps.model),
        format!("serial   {}", caps.serial),
        format!("firmware {}", caps.firmware),
        format!("bus      {}", caps.bus.map(|b| b.name()).unwrap_or("unknown".into())),
    ];
    if caps.ata_security {
        v.push(format!(
            "ATA security: supported, {}{}",
            if caps.ata_frozen { "FROZEN" } else { "not frozen" },
            if caps.ata_security_enabled { ", password set" } else { "" }
        ));
        if let Some((normal, enhanced)) = caps.ata_erase_minutes {
            v.push(format!(
                "  drive estimates {normal} min normal{}",
                if caps.ata_enhanced_erase {
                    format!(", {enhanced} min enhanced")
                } else {
                    String::new()
                }
            ));
        }
    }
    if caps.ata_sanitize {
        let mut kinds = Vec::new();
        if caps.ata_sanitize_crypto { kinds.push("crypto"); }
        if caps.ata_sanitize_block { kinds.push("block"); }
        if caps.ata_sanitize_overwrite { kinds.push("overwrite"); }
        v.push(format!("ATA sanitize: {}", kinds.join(", ")));
    }
    if caps.nvme_format || caps.nvme_sanitize_crypto || caps.nvme_sanitize_block {
        let mut kinds = Vec::new();
        if caps.nvme_format { kinds.push("format"); }
        if caps.nvme_crypto_erase { kinds.push("format-crypto"); }
        if caps.nvme_sanitize_crypto { kinds.push("sanitize-crypto"); }
        if caps.nvme_sanitize_block { kinds.push("sanitize-block"); }
        if caps.nvme_sanitize_overwrite { kinds.push("sanitize-overwrite"); }
        v.push(format!("NVMe: {}", kinds.join(", ")));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay a string down the way a drive does: swapped within each 16-bit word.
    fn put_ata(b: &mut [u8], first: usize, s: &str) {
        for (i, pair) in s.as_bytes().chunks(2).enumerate() {
            let w = first + i;
            b[w * 2] = *pair.get(1).unwrap_or(&b' ');
            b[w * 2 + 1] = pair[0];
        }
    }

    #[test]
    fn ata_strings_are_word_swapped() {
        let mut b = vec![0u8; 512];
        put_ata(&mut b, 27, "WDC WD40EFRX");
        assert_eq!(ata_string(&b, 27, 6), "WDC WD40EFRX");
        // read with the swap ignored and it comes out scrambled, which is the
        // failure this guards against
        assert_ne!(
            String::from_utf8_lossy(&b[54..66]).trim(),
            "WDC WD40EFRX"
        );
    }

    #[test]
    fn methods_are_offered_best_first() {
        let caps = Caps {
            nvme_format: true,
            nvme_crypto_erase: true,
            nvme_sanitize_crypto: true,
            ..Default::default()
        };
        // A crypto sanitize beats a format, which beats nothing.
        assert_eq!(caps.methods()[0], "nvme-sanitize-crypto");
        assert!(caps.blockers().is_empty());
    }

    #[test]
    fn a_frozen_drive_reports_why_it_cannot_erase() {
        let caps = Caps { ata_security: true, ata_frozen: true, ..Default::default() };
        assert!(caps.methods().is_empty(), "a frozen drive offers nothing");
        let why = caps.blockers().join(" ");
        assert!(why.contains("FROZEN"), "must say what to do about it: {why}");
    }

    #[test]
    fn overwrite_sanitize_counts_as_a_method() {
        // An 18 TB enterprise drive offers exactly this and nothing else:
        // frozen security, sanitize overwrite. Omitting it reports the drive
        // as unerasable when it is not.
        let caps = Caps {
            ata_security: true,
            ata_frozen: true,
            ata_sanitize: true,
            ata_sanitize_overwrite: true,
            ..Default::default()
        };
        assert_eq!(caps.methods(), vec!["ata-sanitize-overwrite"]);
        assert!(caps.blockers().is_empty());
    }

    #[test]
    fn sanitize_survives_a_frozen_security_state() {
        // SANITIZE is a separate feature set and is not blocked by the freeze,
        // which is exactly why it is worth preferring.
        let caps = Caps {
            ata_security: true,
            ata_frozen: true,
            ata_sanitize: true,
            ata_sanitize_crypto: true,
            ..Default::default()
        };
        assert_eq!(caps.methods(), vec!["ata-sanitize-crypto"]);
        assert!(caps.blockers().is_empty());
    }

    #[test]
    fn usb_is_called_out_as_untrustworthy() {
        let caps = Caps {
            bus: Some(Bus::Usb),
            nvme_sanitize_crypto: true,
            ..Default::default()
        };
        assert!(caps.blockers().iter().any(|b| b.contains("USB")));
    }
}
