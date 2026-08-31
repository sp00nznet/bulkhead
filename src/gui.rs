//! A plain Win32 window over the commands that recover things.
//!
//! Native controls on purpose: no toolkit, no new dependencies, and it runs
//! anywhere USER32 does -- which includes WinPE, where the recovery media
//! actually needs it.
//!
//! The window does not touch a disk itself. Every button runs bulkhead as a
//! child process and pipes its output into the log, so the GUI cannot get the
//! engine wrong, only the arguments. The destructive ones -- restore, erase --
//! are here, but this window never decides they are safe: it pipes the answer
//! to the engine's own prompt over stdin, so `restore` still wants YES and
//! `erase` still wants the drive's serial, checked against the drive itself.
//! `part move` and `scan --rebuild` stay on the command line.
use std::cell::RefCell;
use std::io::{BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::process::Child;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET, DEFAULT_GUI_FONT,
    FF_DONTCARE, FW_NORMAL, GetStockObject, HBRUSH, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    EM_REPLACESEL, EM_SETSEL, ICC_PROGRESS_CLASS, INITCOMMONCONTROLSEX, InitCommonControlsEx,
    PBM_SETPOS, PBM_SETRANGE32,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::util::{Res, ps, wide};

const ID_LIST: usize = 100;
const ID_IMG: usize = 101;
const ID_LOG: usize = 102;
const ID_DIR: usize = 103;
const ID_PROGRESS: usize = 104;
const ID_STATUS: usize = 105;
const ID_CONFIRM: usize = 106;
const ID_BROWSE_IMG: usize = 110;
const ID_BROWSE_DIR: usize = 111;
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

/// This display's DPI, and the font built for it. Every coordinate in this
/// file is written for 96 DPI and scaled through `dp`; at 200% -- an ordinary
/// laptop setting -- an unscaled window is twice the size it asks for, and the
/// bottom of this one lands well below the screen.
static DPI: AtomicU32 = AtomicU32::new(96);
static FONT: AtomicIsize = AtomicIsize::new(0);

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

struct Ui {
    list: HWND,
    /// Where an image is written to or read from.
    img: HWND,
    /// Where recovered files are written.
    dir: HWND,
    log: HWND,
    /// The bulkhead argument for each row, parallel to the listbox.
    targets: Vec<String>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn append(log: HWND, text: &str) {
    let w = wide(&text.replace('\n', "\r\n"));
    unsafe {
        // Collapse the selection to the end, then "replace" it: the standard
        // way to append to an edit control without rewriting its contents.
        SendMessageW(log, EM_SETSEL, Some(WPARAM(usize::MAX)), Some(LPARAM(-1)));
        SendMessageW(
            log,
            EM_REPLACESEL,
            Some(WPARAM(0)),
            Some(LPARAM(w.as_ptr() as isize)),
        );
    }
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
        let font = FONT.load(Ordering::Relaxed);
        let font = if font == 0 {
            GetStockObject(DEFAULT_GUI_FONT).0 as usize
        } else {
            font as usize
        };
        SendMessageW(h, WM_SETFONT, Some(WPARAM(font)), Some(LPARAM(1)));
        h
    }
}

/// Disks and their volumes, each row carrying the argument that names it.
fn enumerate() -> Vec<(String, String)> {
    let out = ps(
        "Get-Disk | Sort-Object Number | ForEach-Object {
             $d = $_
             \"disk$($d.Number)`tdisk$($d.Number)  $($d.FriendlyName)  $([math]::Round($d.Size/1GB,1)) GB\"
             Get-Partition -DiskNumber $d.Number -ErrorAction SilentlyContinue |
                 Where-Object DriveLetter | ForEach-Object {
                     $v = Get-Volume -DriveLetter $_.DriveLetter -ErrorAction SilentlyContinue
                     \"$($_.DriveLetter):`t    $($_.DriveLetter):  $($v.FileSystem)  $([math]::Round($_.Size/1GB,1)) GB  $($v.FileSystemLabel)\"
                 }
         }",
    ).unwrap_or_default();

    out.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(arg, label)| (arg.trim().to_string(), label.to_string()))
        .collect()
}

/// Repopulate the list and hand back the argument for each row.
///
/// Takes a handle rather than the `Ui`, so no borrow of the shared state is
/// held while messaging controls -- see `wndproc`.
fn refresh(list: HWND) -> Vec<String> {
    unsafe { SendMessageW(list, LB_RESETCONTENT, None, None) };
    let mut targets = Vec::new();
    for (arg, label) in enumerate() {
        let w = wide(&label);
        unsafe {
            SendMessageW(
                list,
                LB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(w.as_ptr() as isize)),
            );
        }
        targets.push(arg);
    }
    targets
}

fn selected(list: HWND, targets: &[String]) -> Option<String> {
    let i = unsafe { SendMessageW(list, LB_GETCURSEL, None, None) }.0;
    if i < 0 {
        return None;
    }
    targets.get(i as usize).cloned()
}

/// Common file dialog. `save` picks the create-a-new-file flavour.
fn pick_file(owner: HWND, save: bool, title: &str) -> Option<String> {
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
        Flags: if save {
            OFN_OVERWRITEPROMPT
        } else {
            OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST
        },
        ..Default::default()
    };
    let ok = unsafe {
        if save {
            GetSaveFileNameW(&mut ofn)
        } else {
            GetOpenFileNameW(&mut ofn)
        }
    };
    if !ok.as_bool() {
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
    String::from_utf16_lossy(&buf[..n])
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

/// The running child, so Cancel can reach it. Only one runs at a time -- the
/// action buttons are disabled for the duration -- so one slot is enough.
static CHILD: Mutex<Option<Child>> = Mutex::new(None);

/// Grey the action buttons out while a command is running, so a second click
/// cannot start a second image writing over the first one's output file.
/// Cancel goes the other way: it is live only while there is something to kill.
fn enable_actions(hwnd: HWND, on: bool) {
    for id in [
        ID_REFRESH,
        ID_SCAN,
        ID_IMAGE,
        ID_MOUNT,
        ID_UNMOUNT,
        ID_UNDELETE,
        ID_CARVE,
        ID_ERASE_INFO,
        ID_ERASE,
        ID_RESTORE,
    ] {
        if let Ok(h) = unsafe { GetDlgItem(Some(hwnd), id as i32) } {
            unsafe {
                let _ = EnableWindow(h, on);
            }
        }
    }
    if let Ok(h) = unsafe { GetDlgItem(Some(hwnd), ID_CANCEL as i32) } {
        unsafe {
            let _ = EnableWindow(h, !on);
        }
    }
}

fn set_progress(bar: HWND, pct: Option<u32>) {
    unsafe {
        SendMessageW(
            bar,
            PBM_SETPOS,
            Some(WPARAM(pct.unwrap_or(0) as usize)),
            None,
        );
    }
}

/// Run bulkhead as a child and stream its output into the log.
///
/// `confirm` is written to the child's stdin. `restore` and `erase` prompt
/// there -- for YES and for the drive's serial respectively -- and this hands
/// the answer over rather than passing `--yes`, so the check that matters
/// still runs in the engine, against the drive, and not in this window.
fn run(log: HWND, args: Vec<String>, confirm: Option<String>) {
    append(log, &format!("\r\n> bulkhead {}\r\n", args.join(" ")));
    let handle = Win(log);
    std::thread::spawn(move || {
        // Bind the wrapper itself before touching its field. Rust 2021
        // captures individual fields where it can, so reaching straight for
        // handle.0 would capture the bare HWND -- which is exactly the
        // non-Send thing the wrapper exists to carry.
        let handle = handle;
        let log = handle.0;
        // The frame owns the title bar, the buttons and the progress bar;
        // the log box is its child.
        let title = unsafe { GetParent(log) }.unwrap_or_default();
        let bar = unsafe { GetDlgItem(Some(title), ID_PROGRESS as i32) }.unwrap_or_default();
        let status = unsafe { GetDlgItem(Some(title), ID_STATUS as i32) }.unwrap_or_default();
        enable_actions(title, false);
        let finish = |msg: &str| {
            set_text(title, "bulkhead");
            set_text(status, "");
            set_progress(bar, None);
            enable_actions(title, true);
            append(log, msg);
        };
        // The engine is always bulkhead.exe next to us. Naming it rather than
        // reusing current_exe() is what lets bulkhead-gui.exe exist: it would
        // otherwise spawn itself and open a second window per button.
        let exe = match std::env::current_exe() {
            Ok(e) => e.with_file_name("bulkhead.exe"),
            Err(e) => return finish(&format!("[!] {e}\r\n")),
        };
        let child = Command::new(exe)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return finish(&format!("[!] {e}\r\n")),
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
                    set_progress(bar, percent(&t));
                    set_text(status, &t);
                    set_text(title, &format!("bulkhead - {t}"));
                } else {
                    set_text(status, &t);
                    append(log, &format!("{t}\r\n"));
                }
            }
        }
        // Killing the child closed the pipe above; take it back to reap it.
        let killed = match CHILD.lock().unwrap().take() {
            Some(mut c) => {
                let st = c.wait();
                st.map(|s| !s.success()).unwrap_or(true)
            }
            // Cancel already took it.
            None => true,
        };
        finish(if killed {
            "-- stopped --\r\n"
        } else {
            "-- done --\r\n"
        });
    });
}

/// Kill whatever is running. Nothing here waits for it: the reader thread sees
/// the pipe close and finishes on its own.
fn cancel(log: HWND) {
    match CHILD.lock().unwrap().as_mut() {
        Some(c) => {
            let _ = c.kill();
            append(
                log,
                "[!] cancelled. The child was killed, so its own cleanup did\r\n",
            );
            append(
                log,
                "    not run: an image may be left attached and a VSS snapshot\r\n",
            );
            append(
                log,
                "    left behind. A half-written image is not a usable backup.\r\n",
            );
        }
        None => append(log, "[!] nothing is running\r\n"),
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

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = wp.0 & 0xFFFF;
            let code = (wp.0 >> 16) & 0xFFFF;
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
                    .map(|ui| (ui.list, ui.img, ui.dir, ui.log, ui.targets.clone()))
            });
            let Some((list, img, dir, log, targets)) = snap else {
                return LRESULT(0);
            };

            let image_path = text_of(img);
            let out_dir = text_of(dir);
            let target = selected(list, &targets);
            let need_target = |verb: &str| -> Option<String> {
                if target.is_none() {
                    append(
                        log,
                        &format!(
                            "[!] pick a disk or volume to {verb}
"
                        ),
                    );
                }
                target.clone()
            };
            let need = |v: &str, what: &str| -> bool {
                let ok = !v.trim().is_empty();
                if !ok {
                    append(
                        log,
                        &format!(
                            "[!] {what}
"
                        ),
                    );
                }
                ok
            };

            match id {
                ID_REFRESH => {
                    let targets = refresh(list);
                    UI.with(|u| {
                        if let Some(ui) = u.borrow_mut().as_mut() {
                            ui.targets = targets;
                        }
                    });
                }
                ID_BROWSE_IMG => {
                    // Saving for a new image, opening for one to mount: the
                    // same box serves both, so offer the create-a-file dialog
                    // and let an existing file be picked from it.
                    if let Some(f) = pick_file(hwnd, true, "Image file") {
                        set_text(img, &f);
                    }
                }
                ID_BROWSE_DIR => {
                    if let Some(f) = pick_folder(hwnd, "Folder to write recovered files into") {
                        set_text(dir, &f);
                    }
                }
                ID_SCAN => {
                    if let Some(t) = need_target("scan") {
                        run(log, vec!["scan".into(), t], None);
                    }
                }
                ID_IMAGE => {
                    if let (Some(t), true) = (
                        need_target("image"),
                        need(&image_path, "choose an image file to write"),
                    ) {
                        run(log, vec!["image".into(), t, image_path.clone()], None);
                    }
                }
                ID_MOUNT => {
                    if need(&image_path, "choose the image file to mount") {
                        run(log, vec!["mount".into(), image_path.clone()], None);
                    }
                }
                ID_UNMOUNT => {
                    if need(&image_path, "choose the image file to unmount") {
                        run(log, vec!["unmount".into(), image_path.clone()], None);
                    }
                }
                ID_CANCEL => cancel(log),
                ID_ERASE_INFO => {
                    if let Some(t) = need_target("inspect") {
                        run(log, vec!["erase-info".into(), t], None);
                    }
                }
                ID_RESTORE => {
                    if let (Some(t), true) = (
                        need_target("restore onto"),
                        need(&image_path, "choose the image file to restore from"),
                    ) && confirm_box(
                        hwnd,
                        &format!(
                            "Restore
{image_path}
onto {t}?

                             Everything on {t} is overwritten. There is no undo."
                        ),
                    ) {
                        run(
                            log,
                            vec!["restore".into(), image_path.clone(), t],
                            Some("YES".into()),
                        );
                    }
                }
                ID_ERASE => {
                    let serial = unsafe { GetDlgItem(Some(hwnd), ID_CONFIRM as i32) }
                        .map(text_of)
                        .unwrap_or_default();
                    if let (Some(t), true) = (
                        need_target("erase"),
                        need(
                            &serial,
                            "type the drive's serial to erase it -- \"Erase info\" prints it",
                        ),
                    ) {
                        // The typed serial is not checked here on purpose. It
                        // goes to the engine, which compares it against the
                        // drive it is about to erase.
                        let mut args = vec!["erase".into(), t.clone()];
                        if checked(hwnd, ID_OVERWRITE) {
                            args.push("--method".into());
                            args.push("overwrite".into());
                        }
                        if confirm_box(hwnd, &format!(
                            "ERASE {t}?

Every sector is destroyed. There is no undo.

                             The serial you typed is checked against the drive;                              if it does not match, nothing happens."))
                        {
                            run(log, args, Some(serial));
                        }
                    }
                }
                ID_UNDELETE => {
                    if let (Some(t), true) = (
                        need_target("recover from"),
                        need(&out_dir, "choose a folder to recover into"),
                    ) {
                        run(
                            log,
                            vec!["undelete".into(), t, "--to".into(), out_dir.clone()],
                            None,
                        );
                    }
                }
                ID_CARVE => {
                    if let (Some(t), true) = (
                        need_target("carve"),
                        need(&out_dir, "choose a folder to recover into"),
                    ) {
                        run(
                            log,
                            vec!["carve".into(), t, "--to".into(), out_dir.clone()],
                            None,
                        );
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let snap = UI.with(|u| {
                u.borrow()
                    .as_ref()
                    .map(|ui| (ui.list, ui.img, ui.dir, ui.log))
            });
            if let Some((list, img, dir, log)) = snap {
                let (w, h) = ((lp.0 & 0xFFFF) as i32, ((lp.0 >> 16) & 0xFFFF) as i32);
                unsafe {
                    let _ = MoveWindow(list, dp(10), dp(25), w - dp(20), dp(150), true);
                    let _ = MoveWindow(img, dp(150), dp(190), w - dp(250), dp(22), true);
                    let _ = MoveWindow(dir, dp(150), dp(220), w - dp(250), dp(22), true);
                    // The two Browse buttons sit at the right-hand edge, so
                    // they move with it -- the edits beside them stretch.
                    for (id, y) in [(ID_BROWSE_IMG, 189), (ID_BROWSE_DIR, 219)] {
                        if let Ok(c) = GetDlgItem(Some(hwnd), id as i32) {
                            let _ = MoveWindow(c, w - dp(95), dp(y), dp(85), dp(24), true);
                        }
                    }
                    for (id, y, cy) in [(ID_PROGRESS, 352, 18), (ID_STATUS, 376, 16)] {
                        if let Ok(c) = GetDlgItem(Some(hwnd), id as i32) {
                            let _ = MoveWindow(c, dp(10), dp(y), w - dp(20), dp(cy), true);
                        }
                    }
                    // On a short screen the log is what gives, but it never
                    // goes negative -- MoveWindow would draw it inside out.
                    let _ = MoveWindow(
                        log,
                        dp(10),
                        dp(398),
                        w - dp(20),
                        (h - dp(408)).max(dp(40)),
                        true,
                    );
                }
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

pub fn run_gui() -> Res<()> {
    unsafe {
        // Before any window: the awareness of a process that already has one
        // cannot be changed. Per-monitor v2 so the frame scales with the
        // monitor it is dragged to.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        DPI.store(GetDpiForSystem(), Ordering::Relaxed);
        // The stock GUI font is a fixed 11px and unreadable once scaled.
        let f = CreateFontW(
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
            FF_DONTCARE.0 as u32,
            w!("Segoe UI"),
        );
        FONT.store(f.0 as isize, Ordering::Relaxed);

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

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("bulkhead"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            dp(760).min(GetSystemMetrics(SM_CXFULLSCREEN)),
            dp(700).min(GetSystemMetrics(SM_CYFULLSCREEN)),
            None,
            None,
            Some(instance.into()),
            None,
        )?;

        control(
            w!("STATIC"),
            w!("Disks and volumes:"),
            0,
            10,
            8,
            300,
            16,
            hwnd,
            0,
        );
        let list = control(
            w!("LISTBOX"),
            PCWSTR::null(),
            (WS_BORDER | WS_VSCROLL).0 | LBS_NOTIFY as u32,
            10,
            25,
            720,
            150,
            hwnd,
            ID_LIST,
        );
        // Two paths, because they are two different things and one box asking
        // for "file/folder" could not say which a given button wanted.
        control(
            w!("STATIC"),
            w!("Image file (.vhdx):"),
            0,
            10,
            193,
            135,
            16,
            hwnd,
            0,
        );
        let img = control(
            w!("EDIT"),
            PCWSTR::null(),
            (WS_BORDER | WS_TABSTOP).0,
            150,
            190,
            510,
            22,
            hwnd,
            ID_IMG,
        );
        control(
            w!("BUTTON"),
            w!("Browse..."),
            WS_TABSTOP.0,
            665,
            189,
            85,
            24,
            hwnd,
            ID_BROWSE_IMG,
        );

        control(
            w!("STATIC"),
            w!("Recover files into:"),
            0,
            10,
            223,
            135,
            16,
            hwnd,
            0,
        );
        let dir = control(
            w!("EDIT"),
            PCWSTR::null(),
            (WS_BORDER | WS_TABSTOP).0,
            150,
            220,
            510,
            22,
            hwnd,
            ID_DIR,
        );
        control(
            w!("BUTTON"),
            w!("Browse..."),
            WS_TABSTOP.0,
            665,
            219,
            85,
            24,
            hwnd,
            ID_BROWSE_DIR,
        );

        // Typed here, checked by the engine against the drive itself.
        control(
            w!("STATIC"),
            w!("Drive serial (to erase):"),
            0,
            10,
            253,
            135,
            16,
            hwnd,
            0,
        );
        control(
            w!("EDIT"),
            PCWSTR::null(),
            (WS_BORDER | WS_TABSTOP).0,
            150,
            250,
            200,
            22,
            hwnd,
            ID_CONFIRM,
        );
        control(
            w!("BUTTON"),
            w!("Overwrite every sector"),
            WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
            365,
            252,
            200,
            20,
            hwnd,
            ID_OVERWRITE,
        );

        let mut x = 10;
        for (id, label, width) in [
            (ID_REFRESH, w!("Refresh"), 80),
            (ID_SCAN, w!("Scan for partitions"), 125),
            (ID_IMAGE, w!("Disk -> image"), 105),
            (ID_MOUNT, w!("Mount image"), 95),
            (ID_UNMOUNT, w!("Unmount"), 80),
            (ID_UNDELETE, w!("Undelete files"), 100),
            (ID_CARVE, w!("Carve files"), 90),
        ] {
            control(
                w!("BUTTON"),
                label,
                WS_TABSTOP.0,
                x,
                284,
                width,
                26,
                hwnd,
                id,
            );
            x += width + 6;
        }

        // Second row, kept apart from the first: everything here writes to a
        // disk, and none of it can be undone.
        let mut x = 10;
        for (id, label, width) in [
            (ID_ERASE_INFO, w!("Erase info"), 90),
            (ID_ERASE, w!("Erase disk"), 90),
            (ID_RESTORE, w!("Restore image -> disk"), 150),
            (ID_CANCEL, w!("Cancel"), 80),
        ] {
            control(
                w!("BUTTON"),
                label,
                WS_TABSTOP.0,
                x,
                316,
                width,
                26,
                hwnd,
                id,
            );
            x += width + 6;
        }

        let bar = control(
            w!("msctls_progress32"),
            PCWSTR::null(),
            0,
            10,
            352,
            740,
            18,
            hwnd,
            ID_PROGRESS,
        );
        SendMessageW(bar, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(100)));
        control(
            w!("STATIC"),
            PCWSTR::null(),
            0,
            10,
            376,
            740,
            16,
            hwnd,
            ID_STATUS,
        );

        let log = control(
            w!("EDIT"),
            PCWSTR::null(),
            (WS_BORDER | WS_VSCROLL).0
                | ES_MULTILINE as u32
                | ES_READONLY as u32
                | ES_AUTOVSCROLL as u32,
            10,
            398,
            740,
            250,
            hwnd,
            ID_LOG,
        );

        let targets = refresh(list);
        UI.with(|u| {
            *u.borrow_mut() = Some(Ui {
                list,
                img,
                dir,
                log,
                targets,
            })
        });

        append(log, "Pick a disk or volume above, then:\r\n");
        append(
            log,
            "  Disk -> image    copies it into the image file (a backup)\r\n",
        );
        append(
            log,
            "  Mount image      attaches an image file as a drive to browse\r\n",
        );
        append(log, "  Unmount          detaches that image again\r\n");
        append(
            log,
            "  Scan             looks for partitions whose table is lost\r\n",
        );
        append(
            log,
            "  Undelete files   recovers deleted files into the folder\r\n",
        );
        append(
            log,
            "  Carve files      last resort: pulls files out by signature,\r\n",
        );
        append(
            log,
            "                   with no names, when no filesystem survives\r\n",
        );
        append(
            log,
            "  Erase info       what erase commands a drive supports\r\n",
        );
        append(
            log,
            "  Erase disk       destroys it. Type the serial on the left first\r\n",
        );
        append(
            log,
            "  Restore          writes an image back over a whole disk\r\n",
        );
        append(
            log,
            "\r\nErase and Restore cannot be undone. Both ask again before they\r\n",
        );
        append(
            log,
            "start, and Erase refuses unless the serial you typed matches the\r\n",
        );
        append(
            log,
            "one the drive reports. part move and scan --rebuild stay on the\r\n",
        );
        append(log, "command line.\r\n");
        let admin = ps("([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)")
            .unwrap_or_default();
        if !admin.trim().eq_ignore_ascii_case("true") {
            append(
                log,
                "\r\n[!] Not elevated. Raw disk access will fail; restart as administrator.\r\n",
            );
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_progress, percent};

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
}
