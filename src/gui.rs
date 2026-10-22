//! A plain Win32 window over the commands that recover things.
//!
//! Native controls on purpose: no toolkit, no new dependencies, and it runs
//! anywhere USER32 does -- which includes WinPE, where the recovery media
//! actually needs it. That is also why there is no common-controls v6
//! manifest: WinPE only has the v5 comctl32 `media` copies in, and a v6
//! dependency would stop the window loading there (see build.rs).
//!
//! The window does not touch a disk itself. Every button runs bulkhead as a
//! child process and pipes its output into the log, so the GUI cannot get the
//! engine wrong, only the arguments. The destructive ones -- restore, erase,
//! rebuild table -- are here, but this window never decides they are safe: it
//! pipes the answer to the engine's own prompt over stdin, so `restore` still
//! wants YES and `erase` still wants the drive's serial, checked against the
//! drive itself. The one exception is "Bring disk online", a Set-Disk call the
//! engine has no command for. `part move` stays on the command line.
//!
//! The engine's output is written for a command line ("Add --rebuild ...",
//! "bulkhead mount ..."). `translate` turns those hints into this window's
//! words; the CLI's own wording is left alone.
use std::cell::RefCell;
use std::io::{BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::process::Child;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET, DEFAULT_GUI_FONT,
    FF_DONTCARE, FIXED_PITCH, FW_NORMAL, GetStockObject, HBRUSH, HFONT, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    EM_REPLACESEL, EM_SETSEL, ICC_PROGRESS_CLASS, INITCOMMONCONTROLSEX, InitCommonControlsEx,
    PBM_SETPOS, PBM_SETRANGE32,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, GetDpiForSystem, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetFocus, GetKeyState, VK_CONTROL,
};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::util::{Res, human, ps, wide};

const ID_LIST: usize = 100;
const ID_IMG: usize = 101;
const ID_LOG: usize = 102;
const ID_DIR: usize = 103;
const ID_PROGRESS: usize = 104;
const ID_STATUS: usize = 105;
const ID_CONFIRM: usize = 106;
const ID_HINT: usize = 107;
const ID_BROWSE_IMG: usize = 110;
const ID_BROWSE_DIR: usize = 111;
const ID_OPEN_DIR: usize = 112;
const ID_REFRESH: usize = 200;
const ID_IMAGE: usize = 201;
const ID_SCAN: usize = 202;
const ID_UNDELETE: usize = 203;
const ID_CARVE: usize = 204;
const ID_MOUNT: usize = 205;
const ID_UNMOUNT: usize = 206;
const ID_CANCEL: usize = 207;
const ID_RESTORE: usize = 208;
const ID_ERASE: usize = 209;
const ID_ERASE_INFO: usize = 210;
const ID_OVERWRITE: usize = 211;
const ID_REBUILD: usize = 212;
const ID_ONLINE: usize = 213;
const ID_ELEVATE: usize = 214;

/// Every button that starts something. Greyed out while a command runs, and
/// each is live only for the kind of row it can act on (`allowed`).
const ACTIONS: [usize; 13] = [
    ID_REFRESH,
    ID_IMAGE,
    ID_MOUNT,
    ID_UNMOUNT,
    ID_SCAN,
    ID_REBUILD,
    ID_UNDELETE,
    ID_CARVE,
    ID_ERASE_INFO,
    ID_ERASE,
    ID_RESTORE,
    ID_ONLINE,
    ID_OPEN_DIR,
];

/// The worker thread's "finished" message to the window. WPARAM is the outcome.
const WM_APP_DONE: u32 = WM_APP + 1;
const DONE: usize = 0;
const FAILED: usize = 1;
const CANCELLED: usize = 2;
const TIMER_ELAPSED: usize = 1;

/// This display's DPI, and the fonts built for it. Every coordinate in this
/// file is written for 96 DPI and scaled through `dp`.
///
/// ponytail: system-DPI aware, not per-monitor. Dragged to a monitor with a
/// different scale, Windows stretches the window rather than it re-laying
/// itself out; handling WM_DPICHANGED (re-create fonts, re-run the layout)
/// is the upgrade.
static DPI: AtomicU32 = AtomicU32::new(96);
static FONT: AtomicIsize = AtomicIsize::new(0);
static MONO: AtomicIsize = AtomicIsize::new(0);

/// Scale a design coordinate to this display.
fn dp(v: i32) -> i32 {
    v * DPI.load(Ordering::Relaxed) as i32 / 96
}

/// Stops a console window flashing up for every child process.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A window handle is fine to use from another thread -- SendMessage marshals
/// to the thread that owns the window -- but the raw pointer inside it is not
/// Send, so say so explicitly rather than restructuring around it.
#[derive(Clone, Copy)]
struct Win(HWND);
unsafe impl Send for Win {}

/// One line of the disk list.
#[derive(Clone, Default)]
struct Row {
    /// What bulkhead is told: `disk2` or `E:`.
    arg: String,
    /// What the list shows.
    label: String,
    /// One line saying exactly which disk this is, for confirmations.
    desc: String,
    is_disk: bool,
    disk: u32,
    serial: String,
    /// Boot or system disk: the running Windows lives here.
    windows: bool,
    offline: bool,
    /// Volume rows: the filesystem, which decides whether undelete applies.
    fs: String,
}

struct Ui {
    list: HWND,
    /// Where an image is written to or read from.
    img: HWND,
    /// Where recovered files are written.
    dir: HWND,
    log: HWND,
    hint: HWND,
    rows: Vec<Row>,
    admin: bool,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn append(log: HWND, text: &str) {
    let w = wide(&text.replace("\r\n", "\n").replace('\n', "\r\n"));
    unsafe {
        // Put the caret at the end, then "replace" the empty selection: the
        // standard way to append to an edit control without rewriting it. The
        // end by length, not by "deselect" (-1): that leaves the caret where it
        // is, and after the help is scrolled to the top that's the start.
        let end = GetWindowTextLengthW(log) as usize;
        SendMessageW(
            log,
            EM_SETSEL,
            Some(WPARAM(end)),
            Some(LPARAM(end as isize)),
        );
        SendMessageW(
            log,
            EM_REPLACESEL,
            Some(WPARAM(0)),
            Some(LPARAM(w.as_ptr() as isize)),
        );
    }
}

fn set_font(h: HWND, font: &AtomicIsize) {
    let f = font.load(Ordering::Relaxed);
    let f = if f == 0 {
        unsafe { GetStockObject(DEFAULT_GUI_FONT).0 as usize }
    } else {
        f as usize
    };
    unsafe { SendMessageW(h, WM_SETFONT, Some(WPARAM(f)), Some(LPARAM(1))) };
}

// A thin wrapper over CreateWindowExW, which takes more arguments than this
// does. Bundling x/y/cx/cy into a struct would add a type that exists only to
// satisfy a lint, at every call site.
#[allow(clippy::too_many_arguments)]
fn control(
    class: PCWSTR,
    text: PCWSTR,
    style: u32,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
    parent: HWND,
    id: usize,
) -> HWND {
    unsafe {
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            text,
            WINDOW_STYLE(style) | WS_CHILD | WS_VISIBLE,
            dp(x),
            dp(y),
            dp(cx),
            dp(cy),
            Some(parent),
            Some(HMENU(id as *mut _)),
            None,
            None,
        )
        .unwrap_or_default();
        set_font(h, &FONT);
        h
    }
}

fn button(label: PCWSTR, x: i32, y: i32, cx: i32, parent: HWND, id: usize) -> HWND {
    control(w!("BUTTON"), label, WS_TABSTOP.0, x, y, cx, 26, parent, id)
}

fn label(text: PCWSTR, x: i32, y: i32, cx: i32, parent: HWND) -> HWND {
    control(w!("STATIC"), text, 0, x, y, cx, 16, parent, 0)
}

/// Disks and their volumes, from Get-Disk / Get-Partition / Get-Volume.
fn enumerate() -> Vec<Row> {
    let out = ps("Get-Disk | Sort-Object Number | ForEach-Object {
             $d = $_
             $bus = \"$($d.BusType)\"
             $loc = if ($bus -eq 'File Backed Virtual') { $d.Location } else { '' }
             \"D`t$($d.Number)`t$($d.FriendlyName)`t$($d.Size)`t$bus`t$(\"$($d.SerialNumber)\".Trim())`t$([int]($d.IsBoot -or $d.IsSystem))`t$([int]$d.IsOffline)`t$loc\"
             Get-Partition -DiskNumber $d.Number -ErrorAction SilentlyContinue |
                 Where-Object DriveLetter | ForEach-Object {
                     $v = Get-Volume -DriveLetter $_.DriveLetter -ErrorAction SilentlyContinue
                     \"V`t$($d.Number)`t$($_.DriveLetter)`t$($v.FileSystem)`t$($_.Size)`t$($v.FileSystemLabel)\"
                 }
         }")
    .unwrap_or_default();
    parse_rows(&out)
}

/// The rows for `enumerate`'s tab-separated lines. Split out so it's testable.
fn parse_rows(out: &str) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.trim_end_matches('\r').split('\t').collect();
        match f.as_slice() {
            ["D", num, model, size, bus, serial, win, off, loc] => {
                let Ok(disk) = num.parse() else { continue };
                let size = human(size.parse().unwrap_or(0));
                let (what, bus) = if loc.is_empty() {
                    (model.trim().to_string(), bus.to_string())
                } else {
                    (loc.to_string(), "image".to_string())
                };
                let windows = *win == "1";
                let offline = *off == "1";
                let mut tags = String::new();
                if windows {
                    tags += "  [holds Windows]";
                }
                if offline {
                    tags += "  [offline]";
                }
                let ser = if serial.is_empty() {
                    "no serial"
                } else {
                    serial
                };
                rows.push(Row {
                    arg: format!("disk{disk}"),
                    label: format!("disk{disk:<3} {size:>9}  {bus:<6}  {what}  ({ser}){tags}"),
                    desc: format!(
                        "disk{disk}: {}{what}, {size}, {}",
                        if loc.is_empty() { "" } else { "image " },
                        if loc.is_empty() {
                            format!("{bus}, ")
                        } else {
                            String::new()
                        } + &if serial.is_empty() {
                            "no serial number".to_string()
                        } else {
                            format!("serial {serial}")
                        }
                    ),
                    is_disk: true,
                    disk,
                    serial: serial.to_string(),
                    windows,
                    offline,
                    fs: String::new(),
                });
            }
            ["V", num, letter, fs, size, vlabel] => {
                let Ok(disk) = num.parse::<u32>() else {
                    continue;
                };
                let size = human(size.parse().unwrap_or(0));
                let named = if vlabel.is_empty() {
                    String::new()
                } else {
                    format!("  \"{vlabel}\"")
                };
                // The disk row already says if this is where Windows lives.
                let windows = rows
                    .iter()
                    .any(|r| r.is_disk && r.disk == disk && r.windows);
                rows.push(Row {
                    arg: format!("{letter}:"),
                    label: format!("    {letter}:  {size:>9}  {fs:<6}{named}"),
                    desc: format!("{letter}: on disk{disk}, {fs}, {size}{named}"),
                    is_disk: false,
                    disk,
                    windows,
                    fs: fs.to_string(),
                    ..Default::default()
                });
                // A disk's description names its volumes, so a confirmation
                // says "E: Data" rather than only "disk2".
                if let Some(d) = rows.iter_mut().find(|r| r.is_disk && r.disk == disk) {
                    if d.desc.contains(", volumes ") {
                        d.desc += &format!(" {letter}:");
                    } else {
                        d.desc += &format!(", volumes {letter}:");
                    }
                }
            }
            _ => {}
        }
    }
    rows
}

/// Repopulate the list, keeping the selection if that row still exists.
///
/// Takes handles rather than the `Ui`, so no borrow of the shared state is
/// held while messaging controls -- see `wndproc`.
fn refresh(list: HWND, keep: Option<&str>) -> Vec<Row> {
    unsafe { SendMessageW(list, LB_RESETCONTENT, None, None) };
    let rows = enumerate();
    for r in &rows {
        let w = wide(&r.label);
        unsafe {
            SendMessageW(
                list,
                LB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(w.as_ptr() as isize)),
            );
        }
    }
    if let Some(i) = keep.and_then(|k| rows.iter().position(|r| r.arg == k)) {
        unsafe { SendMessageW(list, LB_SETCURSEL, Some(WPARAM(i)), None) };
    }
    rows
}

fn selected(list: HWND, rows: &[Row]) -> Option<Row> {
    let i = unsafe { SendMessageW(list, LB_GETCURSEL, None, None) }.0;
    if i < 0 {
        return None;
    }
    rows.get(i as usize).cloned()
}

/// Which buttons make sense for this row. The engine refuses the rest anyway,
/// but only after the user has clicked and, for Restore, said yes to a warning.
fn allowed(id: usize, row: Option<&Row>) -> bool {
    let disk = row.is_some_and(|r| r.is_disk);
    let sys = row.is_some_and(|r| r.windows);
    match id {
        ID_IMAGE | ID_CARVE => row.is_some(),
        ID_SCAN | ID_ERASE_INFO => disk,
        ID_REBUILD | ID_RESTORE => disk && !sys,
        // No serial, nothing to check the typed one against.
        ID_ERASE => disk && !sys && row.is_some_and(|r| !r.serial.is_empty()),
        ID_ONLINE => disk && row.is_some_and(|r| r.offline),
        ID_UNDELETE => row.is_some_and(|r| !r.is_disk && r.fs.eq_ignore_ascii_case("NTFS")),
        _ => true,
    }
}

/// The line under the list: which disk this is and what can be done with it.
fn hint_for(row: Option<&Row>, admin: bool) -> String {
    let mut s = String::new();
    if !admin {
        s += "Not running as administrator, so no disk can be read. Press Restart as administrator.  ";
    }
    let Some(r) = row else {
        return s + "Pick a disk or volume. This line then says what the buttons can do with it.";
    };
    s += &r.desc;
    s += ".";
    if r.is_disk {
        if r.windows {
            s += " The running Windows is on it, so Restore, Erase and Rebuild table are off.";
        } else if r.serial.is_empty() {
            s += " It reports no serial number, so Erase is off: there is nothing to check which drive it is.";
        }
        if r.offline {
            s += " It's offline: Bring disk online to use its volumes.";
        }
    } else if r.fs.eq_ignore_ascii_case("NTFS") {
        s += " Undelete works here. Recover into a folder on another drive.";
    } else {
        s += " Undelete reads NTFS only; Carve works on any volume.";
    }
    s
}

/// Common file dialog. Open, not Save: the same box picks an image to mount or
/// restore and names a new one to write, and the Save dialog's "replace it?"
/// was the wrong question for the first two. A name that doesn't exist yet
/// can still be typed; `check_new_image` refuses one that does.
fn pick_file(owner: HWND, title: &str) -> Option<String> {
    use windows::Win32::UI::Controls::Dialogs::*;
    let mut buf = [0u16; 520];
    let title = wide(title);
    // Double-NUL terminated pairs of description and pattern.
    let filter: Vec<u16> = "Disk images (*.vhdx)\0*.vhdx\0All files\0*.*\0\0"
        .encode_utf16()
        .collect();
    let ext = wide("vhdx");
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        lpstrDefExt: PCWSTR(ext.as_ptr()),
        Flags: OFN_PATHMUSTEXIST,
        ..Default::default()
    };
    if !unsafe { GetOpenFileNameW(&mut ofn) }.as_bool() {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    (n > 0).then(|| String::from_utf16_lossy(&buf[..n]))
}

fn pick_folder(owner: HWND, title: &str) -> Option<String> {
    use windows::Win32::UI::Shell::{
        BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW,
        SHGetPathFromIDListW,
    };
    let title = wide(title);
    let bi = BROWSEINFOW {
        hwndOwner: owner,
        lpszTitle: PCWSTR(title.as_ptr()),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        ..Default::default()
    };
    unsafe {
        let idl = SHBrowseForFolderW(&bi);
        if idl.is_null() {
            return None;
        }
        let mut buf = [0u16; 260];
        let ok = SHGetPathFromIDListW(idl, &mut buf).as_bool();
        // The shell allocated the id list; nothing else will free it.
        windows::Win32::System::Com::CoTaskMemFree(Some(idl as *const _));
        if !ok {
            return None;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
        (n > 0).then(|| String::from_utf16_lossy(&buf[..n]))
    }
}

fn set_text(h: HWND, s: &str) {
    let w = wide(s);
    unsafe {
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

fn text_of(h: HWND) -> String {
    let n = unsafe { GetWindowTextLengthW(h) } as usize;
    let mut buf = vec![0u16; n + 1];
    unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n]).trim().to_string()
}

/// Why a path typed into one of the boxes can't be used, before a command runs
/// for minutes and then fails on it.
fn bad_path(p: &str) -> Option<String> {
    let path = std::path::Path::new(p);
    if !path.is_absolute() {
        return Some(format!(
            "\"{p}\" isn't a full path. Use one like D:\\recovered."
        ));
    }
    let root: String = p.chars().take(3).collect();
    if !std::path::Path::new(&root).exists() {
        return Some(format!("there is no drive {root}"));
    }
    if p[2..].chars().any(|c| "<>\"|?*:".contains(c)) {
        return Some(format!(
            "\"{p}\" has a character Windows doesn't allow in a path"
        ));
    }
    None
}

/// A new image: a full path, in a folder that exists, not overwriting a file.
fn check_new_image(p: &str) -> Option<String> {
    if let Some(e) = bad_path(p) {
        return Some(e);
    }
    let path = std::path::Path::new(p);
    if !p.to_ascii_lowercase().ends_with(".vhdx") {
        return Some("the image file's name has to end in .vhdx".into());
    }
    if path.exists() {
        return Some(format!(
            "{p} already exists. Choose a new name, or delete the old image first."
        ));
    }
    match path.parent() {
        Some(d) if d.is_dir() => None,
        _ => Some(format!("the folder for {p} doesn't exist")),
    }
}

fn check_existing_image(p: &str) -> Option<String> {
    if p.is_empty() {
        return Some("choose an image file first (Browse...)".into());
    }
    (!std::path::Path::new(p).is_file()).then(|| format!("there is no image file at {p}"))
}

/// A progress redraw rather than a message worth keeping. `Region` prints
/// `  43%  102.0 GB / 238.5 GB`, hundreds of times, into one \r-terminated
/// chunk each; the log would fill with them.
fn is_progress(t: &str) -> bool {
    t.contains('%') && t.starts_with(|c: char| c.is_ascii_digit())
}

/// The percentage out of a progress chunk, for the bar.
fn percent(t: &str) -> Option<u32> {
    t.split_once('%')?.0.trim().parse().ok()
}

/// The engine's line, as this window should show it. `None` hides it.
///
/// The engine talks to a command line: it says "Add --rebuild", "bulkhead
/// mount X", "Set-Disk ...", and prints prompts that this window has already
/// answered. Each becomes what to press here instead.
fn translate(line: &str) -> Option<String> {
    // A prompt has no newline, so whatever the engine printed next lands on
    // the same line after it. Drop the prompt, keep the rest.
    let line = match line.split_once("to continue:") {
        Some((_, rest)) => rest.trim(),
        None => line,
    };
    if line.is_empty() {
        return None;
    }
    let says = |s: &str| line.contains(s);
    Some(if says("(0x80070005)") {
        "[!] Windows refused access. bulkhead has to run as administrator: \
         press Restart as administrator at the top."
            .into()
    } else if says("(0x80070050)") || says("(0x800700B7)") {
        "[!] That image file already exists. Choose a new name, or delete the old one first.".into()
    } else if says("os error 123") || says("(0x8007007B)") {
        "[!] A path isn't valid. Check the image file and the folder boxes.".into()
    } else if says("Add --rebuild to write a partition table") {
        "[*] Nothing was written. Press Rebuild table to write a partition table \
         for what was found."
            .into()
    } else if says("mount it:  bulkhead mount") {
        "    Press Mount image to open it as a drive.".into()
    } else if says("detach with:  bulkhead unmount") {
        "    It's in Explorer now. Press Unmount to detach it.".into()
    } else if says("bring it online with:  Set-Disk") {
        "    Boot from it, or press Bring disk online to use it from here.".into()
    } else if says("DetachVirtualDisk") && says("(0x80070015)") {
        "[!] That image isn't attached, so there's nothing to unmount.".into()
    } else if says("put it back with:  bulkhead undo") {
        // Said again, better, by the "table rebuilt" line at the end.
        return None;
    } else if let Some((_, cmd)) = line.split_once("If it is wrong:  ") {
        // No button for this one: undo is rare, and it takes a file. So the
        // command stays, but it says where to type it.
        format!(
            "[+] Table rebuilt. If it's wrong, put the old one back from an\n    \
             administrator command prompt:  {cmd}"
        )
    } else if line == "[!] cancelled" {
        "[!] Stopped before writing anything: the confirmation didn't match. \
         For Erase, the serial you type must be exactly what Erase info prints."
            .into()
    } else {
        line.to_string()
    })
}

/// The running child, so Cancel can reach it. Only one runs at a time -- the
/// action buttons are disabled for the duration -- so one slot is enough.
static CHILD: Mutex<Option<Child>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);
static CANCEL_ASKED: AtomicBool = AtomicBool::new(false);
/// The last thing the engine said, and when: the status line shows it with a
/// clock until a percentage arrives, so a long silent step (taking a disk
/// offline takes half a minute) doesn't look like a hang.
static STAGE: Mutex<Option<(String, Instant)>> = Mutex::new(None);
static SAW_PERCENT: AtomicBool = AtomicBool::new(false);

/// Grey out what doesn't fit the selected row, and everything while a command
/// runs, so a second click cannot start a second image writing over the
/// first one's output file. Cancel goes the other way.
fn update_buttons(hwnd: HWND, row: Option<&Row>) {
    let running = RUNNING.load(Ordering::Relaxed);
    let set = |id: usize, on: bool| {
        if let Ok(h) = unsafe { GetDlgItem(Some(hwnd), id as i32) } {
            unsafe {
                let _ = EnableWindow(h, on);
            }
        }
    };
    for id in ACTIONS {
        set(id, !running && allowed(id, row));
    }
    set(ID_CANCEL, running);
}

fn set_progress(bar: HWND, pct: u32) {
    unsafe { SendMessageW(bar, PBM_SETPOS, Some(WPARAM(pct as usize)), None) };
}

/// Run bulkhead as a child and stream its output into the log.
///
/// `confirm` is written to the child's stdin. `restore`, `scan --rebuild` and
/// `erase` prompt there -- for YES, and for the drive's serial -- and this
/// hands the answer over rather than passing `--yes`, so the check that
/// matters still runs in the engine, against the drive, and not in this window.
fn run(frame: HWND, log: HWND, args: Vec<String>, confirm: Option<String>) {
    append(log, &format!("\n> bulkhead {}\n", args.join(" ")));
    RUNNING.store(true, Ordering::Relaxed);
    CANCEL_ASKED.store(false, Ordering::Relaxed);
    SAW_PERCENT.store(false, Ordering::Relaxed);
    // Named after the command, so a slow start (restore spends half a minute
    // taking the disk offline before it prints anything) says what's slow.
    let verb = args.first().cloned().unwrap_or_default();
    *STAGE.lock().unwrap() = Some((format!("Starting {verb}"), Instant::now()));
    // The last command's "Done." would otherwise sit in the title meanwhile.
    set_text(frame, &format!("bulkhead - {verb}"));
    let bar = unsafe { GetDlgItem(Some(frame), ID_PROGRESS as i32) }.unwrap_or_default();
    set_progress(bar, 0);
    let row = UI.with(|u| {
        u.borrow()
            .as_ref()
            .and_then(|ui| selected(ui.list, &ui.rows))
    });
    update_buttons(frame, row.as_ref());
    unsafe { SetTimer(Some(frame), TIMER_ELAPSED, 1000, None) };

    let handles = (Win(frame), Win(log));
    std::thread::spawn(move || {
        // Bind the wrapper itself before touching its field. Rust 2021
        // captures individual fields where it can, so reaching straight for
        // handle.0 would capture the bare HWND -- which is exactly the
        // non-Send thing the wrapper exists to carry.
        let handles = handles;
        let (frame, log) = (handles.0.0, handles.1.0);
        let bar = unsafe { GetDlgItem(Some(frame), ID_PROGRESS as i32) }.unwrap_or_default();
        let status = unsafe { GetDlgItem(Some(frame), ID_STATUS as i32) }.unwrap_or_default();
        let done = |outcome: usize| unsafe {
            let _ = PostMessageW(Some(frame), WM_APP_DONE, WPARAM(outcome), LPARAM(0));
        };
        // The engine is always bulkhead.exe next to us. Naming it rather than
        // reusing current_exe() is what lets bulkhead-gui.exe exist: it would
        // otherwise spawn itself and open a second window per button.
        let exe = match std::env::current_exe() {
            Ok(e) => e.with_file_name("bulkhead.exe"),
            Err(e) => {
                append(log, &format!("[!] {e}\n"));
                return done(FAILED);
            }
        };
        let mut cmd = Command::new(&exe);
        // A known working directory: scan --rebuild saves the old table in
        // it, and an elevated window otherwise starts in System32.
        if let Some(d) = exe.parent() {
            cmd.current_dir(d);
        }
        let child = cmd
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                append(
                    log,
                    &format!(
                        "[!] can't start {}: {e}\n    It has to sit next to this program.\n",
                        exe.display()
                    ),
                );
                return done(FAILED);
            }
        };
        // Drop stdin either way. Left open, a command that prompts would wait
        // for an answer no one can type; closed, it reads EOF and cancels.
        if let Some(mut si) = child.stdin.take()
            && let Some(c) = &confirm
        {
            let _ = si.write_all(format!("{c}\n").as_bytes());
        }
        let err = child.stderr.take();
        *CHILD.lock().unwrap() = Some(child);

        if let Some(err) = err {
            // Progress redraws with a bare carriage return and no newline, so
            // `lines()` would hold a whole partition in its buffer and show
            // nothing until it finished. Split on both terminators instead.
            let mut rd = BufReader::new(err);
            let mut chunk = Vec::new();
            let mut b = [0u8; 1];
            while rd.read(&mut b).unwrap_or(0) == 1 {
                if b[0] != b'\r' && b[0] != b'\n' {
                    chunk.push(b[0]);
                    continue;
                }
                let t = String::from_utf8_lossy(&chunk).trim().to_string();
                chunk.clear();
                if t.is_empty() {
                    continue;
                }
                // A text box cannot redraw in place, so progress drives the bar
                // and the status line, and the title bar so it stays readable
                // when the window is minimised.
                if is_progress(&t) {
                    SAW_PERCENT.store(true, Ordering::Relaxed);
                    set_progress(bar, percent(&t).unwrap_or(0));
                    let stage = STAGE
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|s| s.0.clone())
                        .unwrap_or_default();
                    set_text(status, &format!("{stage}  -  {t}"));
                    set_text(frame, &format!("bulkhead - {t}"));
                } else if let Some(m) = translate(&t) {
                    let stage = m.trim_start_matches("[*] ").trim_start_matches("[+] ");
                    *STAGE.lock().unwrap() = Some((stage.to_string(), Instant::now()));
                    SAW_PERCENT.store(false, Ordering::Relaxed);
                    set_text(status, stage);
                    append(log, &format!("{m}\n"));
                }
            }
        }
        // Killing the child closed the pipe above; take it back to reap it.
        let ok = match CHILD.lock().unwrap().take() {
            Some(mut c) => c.wait().map(|s| s.success()).unwrap_or(false),
            None => false,
        };
        done(if CANCEL_ASKED.load(Ordering::Relaxed) {
            CANCELLED
        } else if ok {
            DONE
        } else {
            FAILED
        });
    });
}

/// Kill whatever is running. Nothing here waits for it: the reader thread sees
/// the pipe close and reports the ending.
fn cancel(log: HWND) {
    if let Some(c) = CHILD.lock().unwrap().as_mut() {
        CANCEL_ASKED.store(true, Ordering::Relaxed);
        let _ = c.kill();
        append(
            log,
            "[!] Cancelled. bulkhead was stopped mid-command, so its own cleanup didn't run.\n    \
             If an image is still attached, press Unmount with that image file in the box.\n    \
             A half-written image isn't a usable backup: delete it.\n",
        );
    }
}

/// A last look before something irreversible. No is the default button: this
/// is reached by a mouse, and a mouse slips.
fn confirm_box(owner: HWND, text: &str) -> bool {
    let t = wide(text);
    let c = wide("bulkhead");
    unsafe {
        MessageBoxW(
            Some(owner),
            PCWSTR(t.as_ptr()),
            PCWSTR(c.as_ptr()),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        ) == IDYES
    }
}

/// The window failed before there was a window. bulkhead-gui.exe has no
/// console to print to, so a box is the only way this is seen at all.
pub fn fatal(msg: &str) {
    let t = wide(msg);
    let c = wide("bulkhead");
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(t.as_ptr()),
            PCWSTR(c.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn checked(hwnd: HWND, id: usize) -> bool {
    match unsafe { GetDlgItem(Some(hwnd), id as i32) } {
        Ok(h) => unsafe { SendMessageW(h, BM_GETCHECK, None, None) }.0 == 1,
        Err(_) => false,
    }
}

fn shell(owner: HWND, verb: &str, file: &str, params: &str) -> bool {
    let (v, f, p) = (wide(verb), wide(file), wide(params));
    let r = unsafe {
        ShellExecuteW(
            Some(owner),
            PCWSTR(v.as_ptr()),
            PCWSTR(f.as_ptr()),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute's documented success test: anything above 32.
    r.0 as isize > 32
}

/// The same window again, elevated. bulkhead-gui.exe if it's here (its
/// manifest asks for elevation), otherwise this exe's `gui` command.
fn restart_elevated(hwnd: HWND, log: HWND) {
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    let gui = me.with_file_name("bulkhead-gui.exe");
    let started = if gui.is_file() {
        shell(hwnd, "runas", &gui.to_string_lossy(), "")
    } else {
        shell(hwnd, "runas", &me.to_string_lossy(), "gui")
    };
    if started {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    } else {
        append(
            log,
            "[!] Windows didn't start the elevated copy (the prompt was declined?).\n",
        );
    }
}

fn select_changed(hwnd: HWND) {
    let snap = UI.with(|u| {
        u.borrow()
            .as_ref()
            .map(|ui| (ui.hint, selected(ui.list, &ui.rows), ui.admin))
    });
    if let Some((hint, row, admin)) = snap {
        set_text(hint, &hint_for(row.as_ref(), admin));
        update_buttons(hwnd, row.as_ref());
    }
}

/// Re-read the disks and keep the selection: after a mount, an unmount or a
/// restore the list was stale and still showed the old disks.
fn reload(hwnd: HWND) {
    let Some((list, keep)) = UI.with(|u| {
        u.borrow()
            .as_ref()
            .map(|ui| (ui.list, selected(ui.list, &ui.rows).map(|r| r.arg)))
    }) else {
        return;
    };
    let rows = refresh(list, keep.as_deref());
    UI.with(|u| {
        if let Some(ui) = u.borrow_mut().as_mut() {
            ui.rows = rows;
        }
    });
    select_changed(hwnd);
}

fn finished(hwnd: HWND, outcome: usize) {
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_ELAPSED);
    }
    RUNNING.store(false, Ordering::Relaxed);
    let Some(log) = UI.with(|u| u.borrow().as_ref().map(|ui| ui.log)) else {
        return;
    };
    let bar = unsafe { GetDlgItem(Some(hwnd), ID_PROGRESS as i32) }.unwrap_or_default();
    let status = unsafe { GetDlgItem(Some(hwnd), ID_STATUS as i32) }.unwrap_or_default();
    // The ending stays on the status line until the next command, so it can
    // be read after the fact. Done, failed and cancelled each say so.
    let (word, line) = match outcome {
        DONE => {
            set_progress(bar, 100);
            ("Done.", "-- done --")
        }
        CANCELLED => ("Cancelled.", "-- cancelled --"),
        _ => ("Failed. The log says why.", "-- failed --"),
    };
    set_text(status, word);
    set_text(hwnd, &format!("bulkhead - {word}"));
    append(log, &format!("{line}\n"));
    reload(hwnd);
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = wp.0 & 0xFFFF;
            let code = (wp.0 >> 16) & 0xFFFF;
            if id == ID_LIST && code == LBN_SELCHANGE as usize {
                select_changed(hwnd);
                return LRESULT(0);
            }
            // BN_CLICKED is 0. Edit and list boxes report through WM_COMMAND
            // too -- EN_CHANGE, LBN_SELCHANGE -- and appending to the log makes
            // the edit control notify this very window proc, re-entering it
            // from inside our own SendMessage.
            if code != 0 {
                return LRESULT(0);
            }

            // Copy what is needed and release the borrow before touching any
            // control. Holding it across a SendMessage is what turned that
            // re-entry into an abort: a second borrow_mut panics, and a panic
            // cannot unwind out of a window procedure.
            let snap = UI.with(|u| {
                u.borrow()
                    .as_ref()
                    .map(|ui| (ui.list, ui.img, ui.dir, ui.log, ui.rows.clone()))
            });
            let Some((list, img, dir, log, rows)) = snap else {
                return LRESULT(0);
            };

            let image_path = text_of(img);
            let out_dir = text_of(dir);
            let row = selected(list, &rows);
            // Buttons that don't fit the row are greyed out; this catches a
            // click that raced a refresh.
            if !allowed(id, row.as_ref()) {
                return LRESULT(0);
            }
            let say = |m: &str| append(log, &format!("[!] {m}\n"));
            let go = |args: Vec<&str>, confirm: Option<String>| {
                run(
                    hwnd,
                    log,
                    args.into_iter().map(String::from).collect(),
                    confirm,
                )
            };
            let arg = row.as_ref().map(|r| r.arg.clone()).unwrap_or_default();
            let desc = row.as_ref().map(|r| r.desc.clone()).unwrap_or_default();

            match id {
                ID_REFRESH => reload(hwnd),
                ID_ELEVATE => restart_elevated(hwnd, log),
                ID_BROWSE_IMG => {
                    if let Some(f) = pick_file(
                        hwnd,
                        "Pick an image to mount or restore, or type a new name to image into",
                    ) {
                        set_text(img, &f);
                    }
                }
                ID_BROWSE_DIR => {
                    if let Some(f) = pick_folder(
                        hwnd,
                        "Folder to write recovered files into. It must be on another drive.",
                    ) {
                        set_text(dir, &f);
                    }
                }
                ID_OPEN_DIR => {
                    if !std::path::Path::new(&out_dir).is_dir() {
                        say("there's no folder to open yet: choose one under Recover files into");
                    } else {
                        shell(hwnd, "open", &out_dir, "");
                    }
                }
                ID_SCAN => go(vec!["scan", &arg], None),
                ID_REBUILD => {
                    if confirm_box(
                        hwnd,
                        &format!(
                            "Write a new partition table on\n{desc}?\n\n\
                             It points at the filesystems Scan finds. File contents aren't \
                             touched, and the old table is saved first next to bulkhead.exe, \
                             so it can be put back with bulkhead undo."
                        ),
                    ) {
                        go(vec!["scan", &arg, "--rebuild"], Some("YES".into()));
                    }
                }
                ID_IMAGE => match check_new_image(&image_path) {
                    Some(e) => say(&e),
                    None => go(vec!["image", &arg, &image_path], None),
                },
                ID_MOUNT => match check_existing_image(&image_path) {
                    Some(e) => say(&e),
                    None => go(vec!["mount", &image_path], None),
                },
                ID_UNMOUNT => match check_existing_image(&image_path) {
                    Some(e) => say(&e),
                    None => go(vec!["unmount", &image_path], None),
                },
                ID_CANCEL => cancel(log),
                ID_ERASE_INFO => go(vec!["erase-info", &arg], None),
                ID_RESTORE => match check_existing_image(&image_path) {
                    Some(e) => say(&e),
                    None => {
                        if confirm_box(
                            hwnd,
                            &format!(
                                "Restore\n{image_path}\nonto\n{desc}?\n\n\
                                 Everything on that disk is overwritten. There is no undo."
                            ),
                        ) {
                            go(vec!["restore", &image_path, &arg], Some("YES".into()));
                        }
                    }
                },
                ID_ERASE => {
                    let serial = unsafe { GetDlgItem(Some(hwnd), ID_CONFIRM as i32) }
                        .map(text_of)
                        .unwrap_or_default();
                    if serial.is_empty() {
                        say("type the drive's serial in the box first. Erase info prints it.");
                    } else {
                        // The typed serial is not checked here on purpose. It
                        // goes to the engine, which compares it against the
                        // drive it is about to erase.
                        let mut args = vec!["erase", &arg];
                        if checked(hwnd, ID_OVERWRITE) {
                            args.extend(["--method", "overwrite"]);
                        }
                        if confirm_box(
                            hwnd,
                            &format!(
                                "ERASE\n{desc}?\n\nEvery sector is destroyed. There is no undo.\n\n\
                                 The serial you typed is checked against the drive; if it \
                                 doesn't match, nothing happens."
                            ),
                        ) {
                            go(args, Some(serial));
                        }
                    }
                }
                ID_ONLINE => {
                    if confirm_box(
                        hwnd,
                        &format!(
                            "Bring\n{desc}\nonline?\n\nWindows mounts its volumes and gives them \
                             drive letters. If the image it was restored from is still mounted, \
                             unmount that first: Windows may otherwise give the restored disk's \
                             partitions new IDs, which can stop it booting."
                        ),
                    ) {
                        let n = row.as_ref().map(|r| r.disk).unwrap_or(u32::MAX);
                        match ps(&format!("Set-Disk -Number {n} -IsOffline $false")) {
                            Ok(_) => append(log, &format!("[+] disk{n} is online\n")),
                            Err(e) => say(&format!("disk{n} didn't come online: {e}")),
                        }
                        reload(hwnd);
                    }
                }
                ID_UNDELETE | ID_CARVE => match bad_path(&out_dir) {
                    Some(e) if out_dir.is_empty() => {
                        let _ = e;
                        say("choose a folder to recover into, on another drive (Browse...)")
                    }
                    Some(e) => say(&e),
                    None => go(
                        vec![
                            if id == ID_UNDELETE {
                                "undelete"
                            } else {
                                "carve"
                            },
                            &arg,
                            "--to",
                            &out_dir,
                        ],
                        None,
                    ),
                },
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_DONE => {
            finished(hwnd, wp.0);
            LRESULT(0)
        }
        WM_TIMER => {
            if !SAW_PERCENT.load(Ordering::Relaxed)
                && let Some((stage, at)) = STAGE.lock().unwrap().clone()
                && let Ok(status) = unsafe { GetDlgItem(Some(hwnd), ID_STATUS as i32) }
            {
                let s = at.elapsed().as_secs();
                set_text(
                    status,
                    &format!("{stage}  -  working, {}:{:02}", s / 60, s % 60),
                );
            }
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            // Below this the log vanishes and the last buttons are cut off.
            let mm = lp.0 as *mut MINMAXINFO;
            unsafe {
                (*mm).ptMinTrackSize.x = dp(700);
                (*mm).ptMinTrackSize.y = dp(640);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let snap = UI.with(|u| {
                u.borrow()
                    .as_ref()
                    .map(|ui| (ui.list, ui.img, ui.dir, ui.log, ui.hint))
            });
            if let Some((list, img, dir, log, hint)) = snap {
                let (w, h) = ((lp.0 & 0xFFFF) as i32, ((lp.0 >> 16) & 0xFFFF) as i32);
                let mv = |c: HWND, x: i32, y: i32, cx: i32, cy: i32| unsafe {
                    let _ = MoveWindow(c, x, y, cx, cy, true);
                };
                let item =
                    |id: usize| unsafe { GetDlgItem(Some(hwnd), id as i32) }.unwrap_or_default();
                mv(list, dp(10), dp(32), w - dp(20), dp(150));
                mv(hint, dp(10), dp(186), w - dp(20), dp(32));
                mv(img, dp(150), dp(224), w - dp(250), dp(22));
                mv(item(ID_BROWSE_IMG), w - dp(95), dp(223), dp(85), dp(24));
                mv(dir, dp(150), dp(254), w - dp(345), dp(22));
                mv(item(ID_BROWSE_DIR), w - dp(190), dp(253), dp(85), dp(24));
                mv(item(ID_OPEN_DIR), w - dp(95), dp(253), dp(85), dp(24));
                mv(item(ID_REFRESH), w - dp(95), dp(4), dp(85), dp(24));
                mv(item(ID_ELEVATE), w - dp(270), dp(4), dp(170), dp(24));
                mv(item(ID_PROGRESS), dp(10), dp(420), w - dp(115), dp(18));
                mv(item(ID_CANCEL), w - dp(95), dp(416), dp(85), dp(26));
                mv(item(ID_STATUS), dp(10), dp(446), w - dp(20), dp(16));
                // On a short screen the log is what gives, but it never goes
                // negative -- MoveWindow would draw it inside out.
                mv(log, dp(10), dp(468), w - dp(20), (h - dp(478)).max(dp(40)));
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn make_font(face: PCWSTR, pitch: u32) -> HFONT {
    unsafe {
        CreateFontW(
            -dp(12),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            pitch | FF_DONTCARE.0 as u32,
            face,
        )
    }
}

/// The help that opens in the log: what each group of buttons is for.
const HELP: &str = "How to use this:
  1. Pick a disk or volume in the list. The line under it says which disk it
     is and what the buttons can do with it.
  2. Press a button. What bulkhead does shows here; the bar shows progress.

Back up      Disk -> image copies the selection into the image file.
             Mount image opens an image as a drive; Unmount closes it.
Recover      Scan looks for partitions whose table is lost; Rebuild table
             writes one pointing at them (the old table is saved first).
             Undelete and Carve write what they find into the folder,
             which has to be on another drive.
Overwrite    Restore and Erase replace a whole disk and can't be undone.
             Both ask first, and Erase also needs the drive's serial
             (Erase info prints it).
";

pub fn run_gui() -> Res<()> {
    unsafe {
        // Before any window: the awareness of a process that already has one
        // cannot be changed.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        DPI.store(GetDpiForSystem(), Ordering::Relaxed);
        // The stock GUI font is a fixed 11px and unreadable once scaled. The
        // list and the log are monospaced so their columns line up.
        FONT.store(make_font(w!("Segoe UI"), 0).0 as isize, Ordering::Relaxed);
        MONO.store(
            make_font(w!("Consolas"), FIXED_PITCH.0 as u32).0 as isize,
            Ordering::Relaxed,
        );

        // Registers msctls_progress32. Without it the progress bar silently
        // fails to create and every SendMessage to it goes nowhere.
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_PROGRESS_CLASS,
        };
        let _ = InitCommonControlsEx(&icc);
        let instance = GetModuleHandleW(None)?;
        let class = w!("bulkhead_main");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            // COLOR_BTNFACE + 1, the documented way to ask for the system
            // button face as a class background brush.
            hbrBackground: HBRUSH(16 as *mut _),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err("could not register the window class".into());
        }

        // Inside the work area, so the bottom never sits under the taskbar.
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let (ww, wh) = (work.right - work.left, work.bottom - work.top);
        let (cx, cy) = (dp(780).min(ww), dp(760).min(wh));
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class,
            w!("bulkhead"),
            WS_OVERLAPPEDWINDOW,
            work.left + (ww - cx) / 2,
            work.top + (wh - cy) / 2,
            cx,
            cy,
            None,
            None,
            Some(instance.into()),
            None,
        )?;

        let admin = IsUserAnAdmin().as_bool();
        label(w!("Disks and volumes:"), 10, 8, 200, hwnd);
        button(w!("Refresh"), 665, 4, 85, hwnd, ID_REFRESH);
        if !admin {
            button(
                w!("Restart as administrator"),
                490,
                4,
                170,
                hwnd,
                ID_ELEVATE,
            );
        }
        let list = control(
            w!("LISTBOX"),
            PCWSTR::null(),
            (WS_BORDER | WS_VSCROLL | WS_TABSTOP).0 | LBS_NOTIFY as u32,
            10,
            32,
            740,
            150,
            hwnd,
            ID_LIST,
        );
        set_font(list, &MONO);
        let hint = control(
            w!("STATIC"),
            PCWSTR::null(),
            0,
            10,
            186,
            740,
            32,
            hwnd,
            ID_HINT,
        );

        // Two paths, because they are two different things and one box asking
        // for "file/folder" could not say which a given button wanted.
        label(w!("Image file (.vhdx):"), 10, 227, 135, hwnd);
        let img = control(
            w!("EDIT"),
            PCWSTR::null(),
            (WS_BORDER | WS_TABSTOP).0 | ES_AUTOHSCROLL as u32,
            150,
            224,
            510,
            22,
            hwnd,
            ID_IMG,
        );
        button(w!("Browse..."), 665, 223, 85, hwnd, ID_BROWSE_IMG);
        label(w!("Recover files into:"), 10, 257, 135, hwnd);
        let dir = control(
            w!("EDIT"),
            PCWSTR::null(),
            (WS_BORDER | WS_TABSTOP).0 | ES_AUTOHSCROLL as u32,
            150,
            254,
            415,
            22,
            hwnd,
            ID_DIR,
        );
        button(w!("Browse..."), 570, 253, 85, hwnd, ID_BROWSE_DIR);
        button(w!("Open folder"), 665, 253, 85, hwnd, ID_OPEN_DIR);

        // Typed here, checked by the engine against the drive itself.
        label(w!("Drive serial (to erase):"), 10, 287, 135, hwnd);
        control(
            w!("EDIT"),
            PCWSTR::null(),
            (WS_BORDER | WS_TABSTOP).0 | ES_AUTOHSCROLL as u32,
            150,
            284,
            200,
            22,
            hwnd,
            ID_CONFIRM,
        );
        control(
            w!("BUTTON"),
            w!("Overwrite instead (slower; for drives with no firmware erase)"),
            WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
            360,
            285,
            390,
            20,
            hwnd,
            ID_OVERWRITE,
        );

        // Grouped by what they risk. The last row replaces whole disks and is
        // kept away from Cancel, which sits by the progress bar.
        for (y, title, buttons) in [
            (
                318,
                w!("Back up:"),
                &[
                    (ID_IMAGE, w!("Disk -> image"), 110),
                    (ID_MOUNT, w!("Mount image"), 100),
                    (ID_UNMOUNT, w!("Unmount"), 85),
                ][..],
            ),
            (
                350,
                w!("Recover:"),
                &[
                    (ID_SCAN, w!("Scan for partitions"), 130),
                    (ID_REBUILD, w!("Rebuild table"), 100),
                    (ID_UNDELETE, w!("Undelete files"), 105),
                    (ID_CARVE, w!("Carve files"), 90),
                    (ID_ERASE_INFO, w!("Erase info"), 85),
                ][..],
            ),
            (
                382,
                w!("Overwrites a disk:"),
                &[
                    (ID_RESTORE, w!("Restore image -> disk"), 150),
                    (ID_ERASE, w!("Erase disk"), 90),
                    (ID_ONLINE, w!("Bring disk online"), 125),
                ][..],
            ),
        ] {
            label(title, 10, y + 5, 110, hwnd);
            let mut x = 125;
            for &(id, text, width) in buttons {
                button(text, x, y, width, hwnd, id);
                x += width + 6;
            }
        }

        let bar = control(
            w!("msctls_progress32"),
            PCWSTR::null(),
            0,
            10,
            420,
            645,
            18,
            hwnd,
            ID_PROGRESS,
        );
        SendMessageW(bar, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(100)));
        button(w!("Cancel"), 665, 416, 85, hwnd, ID_CANCEL);
        control(
            w!("STATIC"),
            PCWSTR::null(),
            0,
            10,
            446,
            740,
            16,
            hwnd,
            ID_STATUS,
        );

        let log = control(
            w!("EDIT"),
            PCWSTR::null(),
            (WS_BORDER | WS_VSCROLL | WS_TABSTOP).0
                | ES_MULTILINE as u32
                | ES_READONLY as u32
                | ES_AUTOVSCROLL as u32,
            10,
            468,
            740,
            250,
            hwnd,
            ID_LOG,
        );
        set_font(log, &MONO);

        let rows = refresh(list, None);
        UI.with(|u| {
            *u.borrow_mut() = Some(Ui {
                list,
                img,
                dir,
                log,
                hint,
                rows,
                admin,
            })
        });

        append(log, HELP);
        if !admin {
            append(
                log,
                "\n[!] Not running as administrator: every button that reads a disk will be\n    \
                 refused. Press Restart as administrator at the top.\n",
            );
        }
        // Start at the top: the first lines of the help are the ones to read.
        SendMessageW(log, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(0)));
        SendMessageW(log, WM_VSCROLL, Some(WPARAM(SB_TOP.0 as usize)), None);
        select_changed(hwnd);
        // Laid out once everything exists; WM_SIZE does the rest from here.
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        SendMessageW(
            hwnd,
            WM_SIZE,
            None,
            Some(LPARAM(((rc.bottom as isize) << 16) | rc.right as isize)),
        );

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            // Ctrl+A in a text box. The v6 edit control does this itself; the
            // v5 one this window is limited to (see the module docs) doesn't.
            if msg.message == WM_KEYDOWN
                && msg.wParam.0 == 'A' as usize
                && GetKeyState(VK_CONTROL.0 as i32) < 0
            {
                let f = GetFocus();
                let mut cls = [0u16; 16];
                let n = GetClassNameW(f, &mut cls) as usize;
                if String::from_utf16_lossy(&cls[..n]).eq_ignore_ascii_case("edit") {
                    SendMessageW(f, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                    continue;
                }
            }
            // Tab and Shift+Tab between controls, Space on a focused button.
            if IsDialogMessageW(hwnd, &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_goes_to_the_title_and_messages_do_not() {
        // What Region actually emits, once trimmed of its leading spaces.
        assert!(is_progress("43%  102.0 GB / 238.5 GB"));
        assert!(is_progress("0%  0 B / 1.0 MB"));
        assert!(is_progress("100%  1000.0 MB / 1000.0 MB"));

        // Everything bulkhead says on purpose has to survive to the log.
        assert!(!is_progress("[*] partition 5 (I:)"));
        assert!(!is_progress("[!] I: snapshot failed"));
        assert!(!is_progress(r"[+] D:\out.vhdx"));
        // A percentage inside a sentence is a message, not a redraw.
        assert!(!is_progress("[*] 12% of the disk is unreadable"));
    }

    #[test]
    fn the_bar_reads_the_percentage_off_both_shapes() {
        // Region's copy progress, and sanitize's bare percentage.
        assert_eq!(percent("43%  102.0 GB / 238.5 GB"), Some(43));
        assert_eq!(percent("0%  0 B / 1.0 MB"), Some(0));
        assert_eq!(percent("100%"), Some(100));
        assert_eq!(percent("7%"), Some(7));
        assert_eq!(percent("no percentage here"), None);
    }

    #[test]
    fn command_line_hints_become_window_words() {
        // An answered prompt disappears, and what followed it on the line stays.
        assert_eq!(translate("Type YES to continue:"), None);
        assert_eq!(
            translate("Type YES to continue: [*] E: locked").as_deref(),
            Some("[*] E: locked")
        );
        let t = |s: &str| translate(s).unwrap();
        assert!(t("[!] open disk: Access is denied. (0x80070005)").contains("administrator"));
        assert!(
            t("[*] read-only. Add --rebuild to write a partition table for the 2 kept.")
                .contains("Rebuild table")
        );
        assert!(t(r"mount it:  bulkhead mount D:\x.vhdx").contains("Mount image"));
        assert!(
            t("Boot it, or bring it online with:  Set-Disk -Number 3 -IsOffline $false")
                .contains("Bring disk online")
        );
        // Nothing in the window's words tells the user to type a command,
        // except undo, which has no button.
        for s in [
            "Add --rebuild",
            "mount it:  bulkhead mount x",
            "detach with:  bulkhead unmount x",
        ] {
            assert!(!t(s).contains("bulkhead "), "{s}");
        }
        assert_eq!(
            translate(r"    put it back with:  bulkhead undo disk2 C:\b.bin"),
            None
        );
        assert!(
            t(r"[+] table rebuilt. If it is wrong:  bulkhead undo disk2 C:\b.bin")
                .contains("administrator command prompt:  bulkhead undo disk2")
        );
        assert!(
            t("[!] DetachVirtualDisk: The device is not ready. (0x80070015)")
                .contains("isn't attached")
        );
        // Ordinary output passes through untouched.
        assert_eq!(t("[+] 3 file(s) written"), "[+] 3 file(s) written");
    }

    #[test]
    fn paths_are_checked_before_anything_runs() {
        assert!(bad_path("recovered").is_some());
        assert!(bad_path(r"C:\ok\fine").is_none());
        assert!(bad_path(r"C:\bad|name").is_some());
        assert!(check_new_image(r"C:\Windows\x.vhd").is_some());
        assert!(check_new_image(r"C:\no-such-folder-here\x.vhdx").is_some());
        assert!(check_existing_image("").is_some());
    }

    #[test]
    fn rows_say_which_disk_is_which() {
        let rows = parse_rows(
            "D\t0\tSamsung SSD 860\t500107862016\tSATA\tS3Z9NB0K\t1\t0\t\n\
             V\t0\tC\tNTFS\t499000000000\tWindows\n\
             D\t2\tMsft Virtual Disk\t1073741824\tFile Backed Virtual\t\t0\t1\tC:\\t\\q.vhdx\n\
             V\t2\tE\tFAT32\t1000000000\t\n",
        );
        assert_eq!(rows.len(), 4);
        let (sys, vhd) = (&rows[0], &rows[2]);
        assert!(sys.windows && sys.label.contains("[holds Windows]"));
        assert!(sys.desc.contains("serial S3Z9NB0K") && sys.desc.contains("volumes C:"));
        assert!(vhd.label.contains(r"image   C:\t\q.vhdx") && vhd.label.contains("[offline]"));
        assert_eq!(
            vhd.desc,
            r"disk2: image C:\t\q.vhdx, 1.0 GB, no serial number, volumes E:"
        );
        // What each row can do.
        assert!(!allowed(ID_ERASE, Some(sys)) && !allowed(ID_RESTORE, Some(sys)));
        assert!(allowed(ID_SCAN, Some(sys)));
        assert!(!allowed(ID_ERASE, Some(vhd)), "no serial, no erase");
        assert!(allowed(ID_RESTORE, Some(vhd)) && allowed(ID_ONLINE, Some(vhd)));
        assert!(allowed(ID_UNDELETE, Some(&rows[1])));
        assert!(!allowed(ID_UNDELETE, Some(&rows[3])), "FAT32");
        assert!(!allowed(ID_SCAN, Some(&rows[1])), "a volume isn't a disk");
        assert!(!allowed(ID_IMAGE, None));
    }
}
