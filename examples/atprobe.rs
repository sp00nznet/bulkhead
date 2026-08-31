//! Is it the opcode, or is it us?
//!
//! `erase-info` reports SANITIZE STATUS coming back ERROR_NOT_SUPPORTED (0x32)
//! on drives that advertise sanitize, through Microsoft's own storahci. That
//! has two very different explanations: Windows refuses to carry opcode 0xB4
//! through ATA pass-through at all, or our request is malformed. Hunting for a
//! drive that supports sanitize only helps in the second case.
//!
//! So send three commands down the identical path and compare. All three are
//! non-destructive: IDENTIFY and READ VERIFY only read, and SANITIZE STATUS is
//! the status query, not a sanitize.
//!
//!   cargo run --example atprobe -- 1        (elevated)
use std::ffi::c_void;
use std::mem::size_of;

use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::Storage::IscsiDisc::{
    ATA_FLAGS_48BIT_COMMAND, ATA_FLAGS_DATA_IN, ATA_FLAGS_DRDY_REQUIRED, ATA_PASS_THROUGH_DIRECT,
    IOCTL_ATA_PASS_THROUGH_DIRECT, IOCTL_SCSI_PASS_THROUGH_DIRECT, SCSI_PASS_THROUGH_DIRECT,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::PCWSTR;

fn main() {
    let n: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1);
    let path = format!(r"\\.\PhysicalDrive{n}");
    let w: Vec<u16> = path.encode_utf16().chain([0]).collect();
    let h = unsafe {
        CreateFileW(
            // Pass-through needs write access on the handle even to read a
            // status register. Nothing below changes the drive.
            PCWSTR(w.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .unwrap_or_else(|e| panic!("open {path}: {e} (elevated?)"));

    println!("probing {path}\n");
    let mut buf = vec![0u8; 512];

    // Control 1: 28-bit PIO data-in. If this fails, pass-through is dead here
    // and nothing below means anything.
    let mut id = [0u8; 8];
    id[5] = 0xA0;
    id[6] = 0xEC; // IDENTIFY DEVICE
    let r = send(
        h,
        "IDENTIFY (0xEC, 28-bit, data-in)",
        ATA_FLAGS_DRDY_REQUIRED as u16 | ATA_FLAGS_DATA_IN as u16,
        [0; 8],
        id,
        Some(&mut buf),
    );

    // Control 2: 48-bit, non-data -- the exact shape SANITIZE STATUS uses,
    // with a harmless opcode. Verifies one sector at LBA 0, writes nothing.
    let mut cur = [0u8; 8];
    let mut prev = [0u8; 8];
    cur[1] = 1; // count low
    prev[1] = 0; // count high
    cur[5] = 0x40; // LBA mode
    cur[6] = 0x42; // READ VERIFY SECTORS EXT
    send(
        h,
        "READ VERIFY EXT (0x42, 48-bit, non-data)",
        ATA_FLAGS_DRDY_REQUIRED as u16 | ATA_FLAGS_48BIT_COMMAND as u16,
        prev,
        cur,
        None,
    );

    // The subject: same flags, same shape, different opcode.
    let mut cur = [0u8; 8];
    let prev = [0u8; 8];
    cur[0] = 0x00; // feature low: SANITIZE STATUS EXT
    cur[5] = 0x40;
    cur[6] = 0xB4; // SANITIZE DEVICE
    send(
        h,
        "SANITIZE STATUS (0xB4, 48-bit, non-data)",
        ATA_FLAGS_DRDY_REQUIRED as u16 | ATA_FLAGS_48BIT_COMMAND as u16,
        prev,
        cur,
        None,
    );

    if r {
        println!("\nIf the two controls passed and only 0xB4 failed with 50/0x32,");
        println!("Windows is blocking the opcode and no drive swap changes that.");
    }

    // The way around an ATA pass-through filter: wrap the same ATA command in
    // a SCSI ATA PASS-THROUGH(16) CDB and send it down the SCSI path instead,
    // letting the driver's SAT layer unwrap it. Same two commands again, so
    // the control tells us whether the tunnel works at all.
    println!("\n-- via SCSI ATA PASS-THROUGH(16), opcode 0x85 --");

    // Control: IDENTIFY. protocol 4 (PIO data-in), EXTEND 0; T_DIR from
    // device, length in the count field.
    let mut cdb = [0u8; 16];
    cdb[0] = 0x85;
    cdb[1] = 4 << 1;
    cdb[2] = 0x0E;
    cdb[6] = 1; // count: one sector
    cdb[13] = 0xA0;
    cdb[14] = 0xEC;
    scsi_ata(h, "IDENTIFY (0xEC) tunnelled", cdb, 512);

    // The subject: SANITIZE STATUS. protocol 3 (non-data), EXTEND 1, and
    // CK_COND so the drive's registers come back in the sense data.
    let mut cdb = [0u8; 16];
    cdb[0] = 0x85;
    cdb[1] = (3 << 1) | 1;
    cdb[2] = 0x20;
    cdb[13] = 0x40;
    cdb[14] = 0xB4;
    scsi_ata(h, "SANITIZE STATUS (0xB4) tunnelled", cdb, 0);

    unsafe {
        let _ = CloseHandle(h);
    }
}

/// One SCSI ATA PASS-THROUGH(16) round trip, with the sense buffer that
/// carries the drive's register response back.
fn scsi_ata(h: windows::Win32::Foundation::HANDLE, label: &str, cdb: [u8; 16], data_len: u32) {
    #[repr(C)]
    struct Req {
        spt: SCSI_PASS_THROUGH_DIRECT,
        sense: [u8; 32],
    }
    let mut data = vec![0u8; data_len.max(1) as usize];
    let mut req = Req {
        spt: SCSI_PASS_THROUGH_DIRECT {
            Length: size_of::<SCSI_PASS_THROUGH_DIRECT>() as u16,
            CdbLength: 16,
            SenseInfoLength: 32,
            // 1 = in, 0 = out, 2 = no transfer.
            DataIn: if data_len > 0 { 1 } else { 2 },
            DataTransferLength: data_len,
            TimeOutValue: 30,
            DataBuffer: if data_len > 0 {
                data.as_mut_ptr() as *mut c_void
            } else {
                std::ptr::null_mut()
            },
            SenseInfoOffset: size_of::<SCSI_PASS_THROUGH_DIRECT>() as u32,
            Cdb: cdb,
            ..Default::default()
        },
        sense: [0u8; 32],
    };
    let mut ret = 0u32;
    let sz = size_of::<Req>() as u32;
    let r = unsafe {
        DeviceIoControl(
            h,
            IOCTL_SCSI_PASS_THROUGH_DIRECT,
            Some(&mut req as *mut _ as *mut c_void),
            sz,
            Some(&mut req as *mut _ as *mut c_void),
            sz,
            Some(&mut ret),
            None,
        )
    };
    match r {
        Ok(()) => {
            // ScsiStatus 2 is CHECK CONDITION, which CK_COND asks for on
            // purpose -- the ATA registers ride back in the sense data.
            // The ATA Status Return descriptor starts at byte 8 of descriptor-
            // format sense: [09, len, extend, error, count_hi, count_lo,
            // lba(31:24), lba(7:0), lba(39:32), lba(15:8), lba(47:40),
            // lba(23:16), device, status].
            let d = &req.sense[8..22];
            println!(
                "  {label}\n    OK  scsi_status={} sense={:02X?}",
                req.spt.ScsiStatus,
                &req.sense[..22]
            );
            if d[0] == 0x09 {
                let count = u16::from_be_bytes([d[4], d[5]]);
                let lba_lo = u16::from_be_bytes([d[9], d[7]]);
                println!(
                    "    ATA regs: status=0x{:02X} error=0x{:02X}{} count=0x{count:04X} lba(15:0)=0x{lba_lo:04X}",
                    d[13],
                    d[3],
                    if d[3] & 0x04 != 0 {
                        " ABRT <- drive refused it"
                    } else {
                        ""
                    }
                );
            }
        }
        Err(e) => println!("  {label}\n    FAIL  {e}"),
    }
}

fn send(
    h: windows::Win32::Foundation::HANDLE,
    label: &str,
    flags: u16,
    prev: [u8; 8],
    cur: [u8; 8],
    data: Option<&mut Vec<u8>>,
) -> bool {
    let len = data.as_ref().map(|d| d.len()).unwrap_or(0);
    let mut apt = ATA_PASS_THROUGH_DIRECT {
        Length: size_of::<ATA_PASS_THROUGH_DIRECT>() as u16,
        AtaFlags: flags,
        TimeOutValue: 30,
        DataTransferLength: len as u32,
        DataBuffer: data
            .map(|d| d.as_mut_ptr() as *mut c_void)
            .unwrap_or(std::ptr::null_mut()),
        PreviousTaskFile: prev,
        CurrentTaskFile: cur,
        ..Default::default()
    };
    let mut ret = 0u32;
    let sz = size_of::<ATA_PASS_THROUGH_DIRECT>() as u32;
    let r = unsafe {
        DeviceIoControl(
            h,
            IOCTL_ATA_PASS_THROUGH_DIRECT,
            Some(&mut apt as *mut _ as *mut c_void),
            sz,
            Some(&mut apt as *mut _ as *mut c_void),
            sz,
            Some(&mut ret),
            None,
        )
    };
    match r {
        Ok(()) => {
            // The IOCTL succeeding only means the driver carried it. The drive's
            // own verdict is in the status/error registers that came back.
            let status = apt.CurrentTaskFile[6];
            let error = apt.CurrentTaskFile[0];
            let aborted = status & 0x01 != 0;
            println!(
                "  {label}\n    OK  status=0x{status:02X} error=0x{error:02X}{}",
                if aborted {
                    "  <- drive ABORTED it (reached the drive, refused)"
                } else {
                    ""
                }
            );
            true
        }
        Err(e) => {
            println!("  {label}\n    FAIL  {e}");
            false
        }
    }
}
