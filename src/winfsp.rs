//! Mount ext4/XFS/HFS+ as a Windows drive, through WinFsp.
//!
//! WinFsp is loaded at runtime rather than linked, so every other command
//! still works on a machine that does not have it -- and building bulkhead
//! needs no SDK. Only this one command requires the driver.
//!
//! WinFsp itself is GPLv3 with an exception for free software; bulkhead is
//! MIT, which that exception covers. A proprietary fork would need a licence
//! from its authors.
// Every function below is an FFI callback whose whole body works on raw
// pointers handed over by the driver. Marking each operation individually adds
// noise without adding information.
#![allow(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HMODULE, STATUS_ACCESS_DENIED, STATUS_END_OF_FILE,
                                 STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SUCCESS};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::util::{wide, Res};

const DLL: &str = r"C:\Program Files (x86)\WinFsp\bin\winfsp-x64.dll";

// --- the shapes WinFsp expects ---------------------------------------------

/// Volume parameters. The bitfield block is one u32 here: Rust has no bitfields
/// and the layout is what matters, not the names.
#[repr(C)]
struct VolumeParams {
    version: u16,
    sector_size: u16,
    sectors_per_allocation_unit: u16,
    max_component_length: u16,
    volume_creation_time: u64,
    volume_serial_number: u32,
    transact_timeout: u32,
    irp_timeout: u32,
    irp_capacity: u32,
    file_info_timeout: u32,
    flags: u32,
    prefix: [u16; 192],
    file_system_name: [u16; 16],
}

impl Default for VolumeParams {
    fn default() -> Self {
        // Arrays this long have no Default; zeroed is what WinFsp expects for
        // the fields we do not set anyway.
        unsafe { std::mem::zeroed() }
    }
}

/// Bit positions inside `VolumeParams::flags`, in declaration order.
const CASE_SENSITIVE_SEARCH: u32 = 1 << 0;
const CASE_PRESERVED_NAMES: u32 = 1 << 1;
const UNICODE_ON_DISK: u32 = 1 << 2;
const READ_ONLY_VOLUME: u32 = 1 << 9;
const UM_FILE_CONTEXT_IS_USER_CONTEXT2: u32 = 1 << 16;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FileInfo {
    file_attributes: u32,
    reparse_tag: u32,
    allocation_size: u64,
    file_size: u64,
    creation_time: u64,
    last_access_time: u64,
    last_write_time: u64,
    change_time: u64,
    index_number: u64,
    hard_links: u32,
    ea_size: u32,
}

#[repr(C)]
struct VolumeInfo {
    total_size: u64,
    free_size: u64,
    volume_label_length: u16,
    volume_label: [u16; 32],
}

impl Default for VolumeInfo {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

/// The volume is read-only; every write path answers with this.
const STATUS_MEDIA_WRITE_PROTECTED: i32 = 0xC000_00A2u32 as i32;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_READONLY: u32 = 0x01;

/// The callback table. Only the entries a read-only filesystem needs are
/// filled in; the rest stay null and WinFsp answers them itself.
#[repr(C)]
struct Interface {
    get_volume_info: Option<unsafe extern "system" fn(*mut c_void, *mut VolumeInfo) -> i32>,
    set_volume_label: *const c_void,
    get_security_by_name: Option<
        unsafe extern "system" fn(*mut c_void, PCWSTR, *mut u32, *mut c_void, *mut u64) -> i32,
    >,
    create: Option<
        unsafe extern "system" fn(*mut c_void, PCWSTR, u32, u32, u32, *mut c_void, u64,
                                  *mut *mut c_void, *mut FileInfo) -> i32,
    >,
    open: Option<
        unsafe extern "system" fn(*mut c_void, PCWSTR, u32, u32, *mut *mut c_void, *mut FileInfo)
            -> i32,
    >,
    overwrite: Option<
        unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u8, u64, *mut FileInfo) -> i32,
    >,
    cleanup: *const c_void,
    close: Option<unsafe extern "system" fn(*mut c_void, *mut c_void)>,
    read: Option<
        unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void, u64, u32, *mut u32) -> i32,
    >,
    write: *const c_void,
    flush: *const c_void,
    get_file_info: Option<unsafe extern "system" fn(*mut c_void, *mut c_void, *mut FileInfo) -> i32>,
    set_basic_info: *const c_void,
    set_file_size: *const c_void,
    can_delete: *const c_void,
    rename: *const c_void,
    get_security: *const c_void,
    set_security: *const c_void,
    read_directory: Option<
        unsafe extern "system" fn(*mut c_void, *mut c_void, PCWSTR, PCWSTR, *mut c_void, u32,
                                  *mut u32) -> i32,
    >,
    resolve_reparse_points: *const c_void,
    get_reparse_point: *const c_void,
    set_reparse_point: *const c_void,
    delete_reparse_point: *const c_void,
    get_stream_info: *const c_void,
    get_dir_info_by_name: *const c_void,
    control: *const c_void,
    set_delete: *const c_void,
    create_ex: *const c_void,
    overwrite_ex: *const c_void,
    get_ea: *const c_void,
    set_ea: *const c_void,
    obsolete0: *const c_void,
    dispatcher_stopped: *const c_void,
    reserved: [*const c_void; 31],
}

impl Default for Interface {
    fn default() -> Self {
        // Zeroed apart from what we implement: a null entry means "not
        // supported", which WinFsp handles.
        unsafe { std::mem::zeroed() }
    }
}

/// One directory entry, as WinFsp wants it: a length-prefixed header followed
/// by the name, with no terminator.
#[repr(C)]
struct DirInfo {
    size: u16,
    file_info: FileInfo,
    padding: [u8; 24],
    // name follows
}

// --- the parts of WinFsp we call --------------------------------------------

type FspCreate = unsafe extern "system" fn(PCWSTR, *const VolumeParams, *const Interface,
                                           *mut *mut c_void) -> i32;
type FspSetMountPoint = unsafe extern "system" fn(*mut c_void, PCWSTR) -> i32;
type FspStartDispatcher = unsafe extern "system" fn(*mut c_void, u32) -> i32;
type FspStopDispatcher = unsafe extern "system" fn(*mut c_void);
type FspDelete = unsafe extern "system" fn(*mut c_void);
type FspAddDirInfo = unsafe extern "system" fn(*mut DirInfo, *mut c_void, u32, *mut u32) -> u8;
type FspSetDebugLog = unsafe extern "system" fn(*mut c_void, u32);
type FspDebugLogSetHandle = unsafe extern "system" fn(isize);

struct Api {
    create: FspCreate,
    set_mount_point: FspSetMountPoint,
    start_dispatcher: FspStartDispatcher,
    stop_dispatcher: FspStopDispatcher,
    delete: FspDelete,
    add_dir_info: FspAddDirInfo,
    /// Optional: only used by --debug, and absent from some builds.
    set_debug_log: Option<FspSetDebugLog>,
    debug_log_set_handle: Option<FspDebugLogSetHandle>,
}

unsafe fn sym(m: HMODULE, name: &str) -> Res<*const c_void> {
    let c = std::ffi::CString::new(name)?;
    let p = GetProcAddress(m, windows::core::PCSTR(c.as_ptr() as *const u8));
    p.map(|f| f as *const c_void)
        .ok_or_else(|| format!("{name} missing from {DLL}").into())
}

fn load() -> Res<Api> {
    unsafe {
        let m = LoadLibraryW(PCWSTR(wide(DLL).as_ptr())).map_err(|e| {
            format!("WinFsp is not installed ({e}).\n    \
                     Get it from https://winfsp.dev or: winget install WinFsp.WinFsp")
        })?;
        Ok(Api {
            create: std::mem::transmute::<*const c_void, FspCreate>(
                sym(m, "FspFileSystemCreate")?),
            set_mount_point: std::mem::transmute::<*const c_void, FspSetMountPoint>(
                sym(m, "FspFileSystemSetMountPoint")?),
            start_dispatcher: std::mem::transmute::<*const c_void, FspStartDispatcher>(
                sym(m, "FspFileSystemStartDispatcher")?),
            stop_dispatcher: std::mem::transmute::<*const c_void, FspStopDispatcher>(
                sym(m, "FspFileSystemStopDispatcher")?),
            delete: std::mem::transmute::<*const c_void, FspDelete>(
                sym(m, "FspFileSystemDelete")?),
            add_dir_info: std::mem::transmute::<*const c_void, FspAddDirInfo>(
                sym(m, "FspFileSystemAddDirInfo")?),
            // SetDebugLog is an inline helper in the header; the exported one
            // is SetDebugLogF. Neither is required to mount.
            set_debug_log: sym(m, "FspFileSystemSetDebugLogF").ok()
                .map(|f| std::mem::transmute::<*const c_void, FspSetDebugLog>(f)),
            debug_log_set_handle: sym(m, "FspDebugLogSetHandle").ok()
                .map(|f| std::mem::transmute::<*const c_void, FspDebugLogSetHandle>(f)),
        })
    }
}

// --- what we are serving ----------------------------------------------------

/// A path resolved to something on the source filesystem.
struct Node {
    id: u64,
    is_dir: bool,
    size: u64,
}

/// Everything the callbacks need, behind one lock.
///
/// One lock, not two, because WinFsp dispatches on its own thread pool and two
/// locks taken in different orders eventually deadlock. It also serialises
/// device access, which is required rather than merely tidy: `Raw::read` seeks
/// and then reads, so concurrent callbacks sharing a handle would each read
/// from the other's file position.
struct Mount {
    fs: crate::FsHandle,
    /// Resolved nodes, keyed by the handle handed back to WinFsp.
    open: HashMap<u64, Node>,
    next_handle: u64,
    label: String,
    total: u64,
}

/// The device handle inside is a raw pointer, so this is not Send on its own.
/// Sharing it is safe because the mutex serialises every use.
unsafe impl Send for Mount {}

static MOUNT: Mutex<Option<Mount>> = Mutex::new(None);

fn to_path(p: PCWSTR) -> String {
    if p.is_null() {
        return "/".into();
    }
    let s = unsafe { p.to_string() }.unwrap_or_default();
    s.replace('\\', "/")
}

/// Windows time is 100ns ticks since 1601; everything here is read-only and
/// undated, so one fixed plausible value beats a wrong one.
const FIXED_TIME: u64 = 133_000_000_000_000_000;

fn fill(info: &mut FileInfo, n: &Node) {
    info.file_attributes =
        FILE_ATTRIBUTE_READONLY | if n.is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 };
    info.file_size = n.size;
    info.allocation_size = n.size.div_ceil(4096) * 4096;
    info.creation_time = FIXED_TIME;
    info.last_access_time = FIXED_TIME;
    info.last_write_time = FIXED_TIME;
    info.change_time = FIXED_TIME;
    info.index_number = n.id;
    info.hard_links = 0;
}

fn resolve(m: &Mount, path: &str) -> Option<Node> {
    let (id, is_dir) = m.fs.resolve(path).ok()?;
    let size = if is_dir { 0 } else { m.fs.size_of(id).unwrap_or(0) };
    Some(Node { id, is_dir, size })
}

/// WinFsp rejects the whole create path unless Create and Overwrite exist,
/// even for opening a file read-only -- it checks all three up front. So a
/// read-only filesystem still has to answer them, and the honest answer is
/// that the volume is write protected.
unsafe extern "system" fn cb_create(
    _fs: *mut c_void, _name: PCWSTR, _opts: u32, _access: u32, _attrs: u32,
    _sd: *mut c_void, _alloc: u64, _context: *mut *mut c_void, _info: *mut FileInfo,
) -> i32 {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_overwrite(
    _fs: *mut c_void, _context: *mut c_void, _attrs: u32, _replace: u8,
    _alloc: u64, _info: *mut FileInfo,
) -> i32 {
    STATUS_MEDIA_WRITE_PROTECTED
}

unsafe extern "system" fn cb_get_volume_info(_fs: *mut c_void, out: *mut VolumeInfo) -> i32 {
    let g = MOUNT.lock().unwrap();
    let Some(st) = g.as_ref() else { return STATUS_ACCESS_DENIED.0 };
    let vi = &mut *out;
    vi.total_size = st.total;
    vi.free_size = 0; // read-only: nothing can be written into it
    let label: Vec<u16> = st.label.encode_utf16().take(31).collect();
    vi.volume_label[..label.len()].copy_from_slice(&label);
    vi.volume_label_length = (label.len() * 2) as u16;
    STATUS_SUCCESS.0
}

unsafe extern "system" fn cb_get_security_by_name(
    _fs: *mut c_void, name: PCWSTR, attributes: *mut u32,
    _sd: *mut c_void, sd_size: *mut u64,
) -> i32 {
    let g = MOUNT.lock().unwrap();
    let Some(m) = g.as_ref() else { return STATUS_ACCESS_DENIED.0 };
    let Some(n) = resolve(m, &to_path(name)) else { return STATUS_OBJECT_NAME_NOT_FOUND.0 };
    if !attributes.is_null() {
        *attributes =
            FILE_ATTRIBUTE_READONLY | if n.is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 };
    }
    // No security descriptor: WinFsp then grants access by its own default.
    if !sd_size.is_null() {
        *sd_size = 0;
    }
    STATUS_SUCCESS.0
}

unsafe extern "system" fn cb_open(
    _fs: *mut c_void, name: PCWSTR, _create_options: u32, _granted_access: u32,
    context: *mut *mut c_void, info: *mut FileInfo,
) -> i32 {
    let mut g = MOUNT.lock().unwrap();
    let Some(st) = g.as_mut() else { return STATUS_ACCESS_DENIED.0 };
    let Some(n) = resolve(st, &to_path(name)) else { return STATUS_OBJECT_NAME_NOT_FOUND.0 };
    fill(&mut *info, &n);
    st.next_handle += 1;
    let h = st.next_handle;
    st.open.insert(h, n);
    *context = h as *mut c_void;
    STATUS_SUCCESS.0
}

unsafe extern "system" fn cb_close(_fs: *mut c_void, context: *mut c_void) {
    if let Ok(mut g) = MOUNT.lock() {
        if let Some(st) = g.as_mut() {
            st.open.remove(&(context as u64));
        }
    }
}

unsafe extern "system" fn cb_get_file_info(
    _fs: *mut c_void, context: *mut c_void, info: *mut FileInfo,
) -> i32 {
    let g = MOUNT.lock().unwrap();
    let Some(st) = g.as_ref() else { return STATUS_ACCESS_DENIED.0 };
    let Some(n) = st.open.get(&(context as u64)) else { return STATUS_OBJECT_NAME_NOT_FOUND.0 };
    fill(&mut *info, n);
    STATUS_SUCCESS.0
}

unsafe extern "system" fn cb_read(
    _fs: *mut c_void, context: *mut c_void, buffer: *mut c_void,
    offset: u64, length: u32, transferred: *mut u32,
) -> i32 {
    let g = MOUNT.lock().unwrap();
    let Some(m) = g.as_ref() else { return STATUS_ACCESS_DENIED.0 };
    let Some(n) = m.open.get(&(context as u64)) else { return STATUS_OBJECT_NAME_NOT_FOUND.0 };
    if offset >= n.size {
        return STATUS_END_OF_FILE.0;
    }
    // ponytail: reads the whole file and slices out the window asked for.
    // Correct, and fine for browsing or copying off; a large file read near
    // its end pays for the whole thing each time. Per-extent reads are the
    // upgrade if that ever matters.
    let Ok(data) = m.fs.read_file(n.id) else { return STATUS_END_OF_FILE.0 };
    let start = offset as usize;
    if start >= data.len() {
        return STATUS_END_OF_FILE.0;
    }
    let end = (start + length as usize).min(data.len());
    let got = end - start;
    std::ptr::copy_nonoverlapping(data[start..end].as_ptr(), buffer as *mut u8, got);
    *transferred = got as u32;
    STATUS_SUCCESS.0
}

unsafe extern "system" fn cb_read_directory(
    _fs: *mut c_void, context: *mut c_void, _pattern: PCWSTR, marker: PCWSTR,
    buffer: *mut c_void, length: u32, transferred: *mut u32,
) -> i32 {
    let Ok(api) = load() else { return STATUS_ACCESS_DENIED.0 };
    let g = MOUNT.lock().unwrap();
    let Some(m) = g.as_ref() else { return STATUS_ACCESS_DENIED.0 };
    let Some(dir) = m.open.get(&(context as u64)).filter(|n| n.is_dir) else {
        return STATUS_OBJECT_NAME_NOT_FOUND.0;
    };
    let entries = m.fs.read_dir(dir.id).unwrap_or_default();

    // WinFsp pages long directories: it hands back the last name it saw and
    // expects the listing to resume after it.
    let after = if marker.is_null() { None } else { marker.to_string().ok() };
    let mut resumed = after.is_none();

    for e in entries {
        if !resumed {
            if Some(&e.name) == after.as_ref() {
                resumed = true;
            }
            continue;
        }
        let name: Vec<u16> = e.name.encode_utf16().collect();
        let mut buf = vec![0u8; std::mem::size_of::<DirInfo>() + name.len() * 2];
        let di = buf.as_mut_ptr() as *mut DirInfo;
        (*di).size = buf.len() as u16;
        let size = if e.is_dir { 0 } else { m.fs.size_of(e.inode).unwrap_or(0) };
        fill(&mut (*di).file_info, &Node { id: e.inode, is_dir: e.is_dir, size });
        std::ptr::copy_nonoverlapping(
            name.as_ptr() as *const u8,
            buf.as_mut_ptr().add(std::mem::size_of::<DirInfo>()),
            name.len() * 2,
        );
        if (api.add_dir_info)(di, buffer, length, transferred) == 0 {
            return STATUS_SUCCESS.0; // buffer full; WinFsp asks again
        }
    }
    // A null entry marks the end of the listing.
    (api.add_dir_info)(std::ptr::null_mut(), buffer, length, transferred);
    STATUS_SUCCESS.0
}

/// The live filesystem handle, so Ctrl-C can take the mount down cleanly.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// Ctrl-C promises to unmount, so it has to actually unmount. Without this the
/// process dies with the mount still registered and the drive letter lingers
/// until WinFsp notices.
unsafe extern "system" fn on_ctrl_c(_kind: u32) -> windows::core::BOOL {
    let h = LIVE.swap(0, Ordering::SeqCst);
    if h != 0 {
        if let Ok(api) = load() {
            (api.stop_dispatcher)(h as *mut c_void);
            (api.delete)(h as *mut c_void);
        }
        eprintln!("
[+] unmounted");
    }
    std::process::exit(0);
}

/// Mount a filesystem at a drive letter or directory, until interrupted.
pub fn mount(fs: crate::FsHandle, mount_point: &str, label: &str, total: u64,
             debug: bool) -> Res<()> {
    let api = load()?;

    *MOUNT.lock().unwrap() = Some(Mount {
        fs,
        open: HashMap::new(),
        next_handle: 0,
        label: label.to_string(),
        total,
    });

    let mut params = VolumeParams {
        // 0 means "the V0 fields only", which is exactly what this struct is.
        // The alternative is the size of the full V1 structure; giving the V0
        // size is neither, and WinFsp then reads past the end of it.
        version: 0,
        sector_size: 512,
        sectors_per_allocation_unit: 1,
        max_component_length: 255,
        volume_creation_time: FIXED_TIME,
        volume_serial_number: 0xB1CB_EAD0,
        file_info_timeout: 1000,
        flags: CASE_PRESERVED_NAMES
            | CASE_SENSITIVE_SEARCH
            | UNICODE_ON_DISK
            | READ_ONLY_VOLUME
            | UM_FILE_CONTEXT_IS_USER_CONTEXT2,
        ..Default::default()
    };
    let name: Vec<u16> = "bulkhead".encode_utf16().collect();
    params.file_system_name[..name.len()].copy_from_slice(&name);

    let iface = Interface {
        get_volume_info: Some(cb_get_volume_info),
        get_security_by_name: Some(cb_get_security_by_name),
        create: Some(cb_create),
        open: Some(cb_open),
        overwrite: Some(cb_overwrite),
        close: Some(cb_close),
        read: Some(cb_read),
        get_file_info: Some(cb_get_file_info),
        read_directory: Some(cb_read_directory),
        ..Default::default()
    };

    let mut handle: *mut c_void = std::ptr::null_mut();
    // FSP_FSCTL_DISK_DEVICE_NAME: the product name plus ".Disk". Not a
    // \Device\ path -- WinFsp builds that itself.
    let device = wide("WinFsp.Disk");
    let st = unsafe { (api.create)(PCWSTR(device.as_ptr()), &params, &iface, &mut handle) };
    if st != 0 {
        return Err(format!("FspFileSystemCreate failed ({st:#x})").into());
    }

    let mp = wide(mount_point);
    let st = unsafe { (api.set_mount_point)(handle, PCWSTR(mp.as_ptr())) };
    if st != 0 {
        unsafe { (api.delete)(handle) };
        return Err(format!(
            "could not mount at {mount_point} ({st:#x}) -- is the letter already in use?"
        ).into());
    }

    if debug {
        // WinFsp names each operation and its result, which is the only way to
        // see which one a filesystem is getting wrong.
        unsafe {
            let stderr = windows::Win32::System::Console::GetStdHandle(
                windows::Win32::System::Console::STD_ERROR_HANDLE)
                .map(|h| h.0 as isize).unwrap_or(0);
            if let Some(f) = api.debug_log_set_handle {
                f(stderr);
            }
            match api.set_debug_log {
                Some(f) => {
                    f(handle, u32::MAX);
                    eprintln!("[*] WinFsp operation logging on");
                }
                None => eprintln!("[!] this WinFsp build exports no debug log"),
            }
        }
    }

    let st = unsafe { (api.start_dispatcher)(handle, 0) };
    if st != 0 {
        unsafe { (api.delete)(handle) };
        return Err(format!("FspFileSystemStartDispatcher failed ({st:#x})").into());
    }

    LIVE.store(handle as usize, Ordering::SeqCst);
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleCtrlHandler(Some(on_ctrl_c), true);
    }

    eprintln!("[+] mounted at {mount_point} (read-only)");
    eprintln!("    browse it in Explorer; press Ctrl-C here to unmount");
    // The dispatcher serves on its own threads; this one just waits.
    loop {
        std::thread::park();
    }
}
