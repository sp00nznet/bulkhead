//! A plain Win32 window over the commands that recover things.
//!
//! Native controls on purpose: no toolkit, no new dependencies, and it runs
//! anywhere USER32 does -- which includes WinPE, where the recovery media
//! actually needs it.
//!
//! The window does not touch a disk itself. Every button runs bulkhead as a
//! child process and pipes its output into the log, so the GUI cannot get the
//! engine wrong, only the arguments. Destructive commands -- restore, part
//! move, scan --rebuild -- are deliberately absent: they belong where their
//! confirmations are, on the command line.
use std::cell::RefCell;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{EM_REPLACESEL, EM_SETSEL};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::util::{ps, wide, Res};

const ID_LIST: usize = 100;
const ID_IMG: usize = 101;
const ID_LOG: usize = 102;
const ID_DIR: usize = 103;
const ID_BROWSE_IMG: usize = 110;
const ID_BROWSE_DIR: usize = 111;
const ID_REFRESH: usize = 200;
const ID_IMAGE: usize = 201;
const ID_SCAN: usize = 202;
const ID_UNDELETE: usize = 203;
const ID_CARVE: usize = 204;
const ID_MOUNT: usize = 205;

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
        SendMessageW(log, EM_REPLACESEL, Some(WPARAM(0)), Some(LPARAM(w.as_ptr() as isize)));
    }
}

fn control(class: PCWSTR, text: PCWSTR, style: u32, x: i32, y: i32, cx: i32, cy: i32,
           parent: HWND, id: usize) -> HWND {
    unsafe {
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0), class, text,
            WINDOW_STYLE(style) | WS_CHILD | WS_VISIBLE,
            x, y, cx, cy, Some(parent), Some(HMENU(id as *mut _)), None, None,
        ).unwrap_or_default();
        SendMessageW(h, WM_SETFONT,
                     Some(WPARAM(GetStockObject(DEFAULT_GUI_FONT).0 as usize)),
                     Some(LPARAM(1)));
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
            SendMessageW(list, LB_ADDSTRING, Some(WPARAM(0)),
                         Some(LPARAM(w.as_ptr() as isize)));
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
        .encode_utf16().collect();
    let ext = wide("vhdx");
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        lpstrDefExt: PCWSTR(ext.as_ptr()),
        Flags: if save { OFN_OVERWRITEPROMPT } else { OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST },
        ..Default::default()
    };
    let ok = unsafe {
        if save { GetSaveFileNameW(&mut ofn) } else { GetOpenFileNameW(&mut ofn) }
    };
    if !ok.as_bool() {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    (n > 0).then(|| String::from_utf16_lossy(&buf[..n]))
}

fn pick_folder(owner: HWND, title: &str) -> Option<String> {
    use windows::Win32::UI::Shell::{SHBrowseForFolderW, SHGetPathFromIDListW,
                                   BROWSEINFOW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS};
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
    unsafe { let _ = SetWindowTextW(h, PCWSTR(w.as_ptr())); }
}

fn text_of(h: HWND) -> String {
    let n = unsafe { GetWindowTextLengthW(h) } as usize;
    let mut buf = vec![0u16; n + 1];
    unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n])
}

/// Run bulkhead as a child and stream its output into the log.
fn run(log: HWND, args: Vec<String>) {
    append(log, &format!("\r\n> bulkhead {}\r\n", args.join(" ")));
    let handle = Win(log);
    std::thread::spawn(move || {
        // Bind the wrapper itself before touching its field. Rust 2021
        // captures individual fields where it can, so reaching straight for
        // handle.0 would capture the bare HWND -- which is exactly the
        // non-Send thing the wrapper exists to carry.
        let handle = handle;
        let log = handle.0;
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => return append(log, &format!("[!] {e}\r\n")),
        };
        let child = Command::new(exe)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return append(log, &format!("[!] {e}\r\n")),
        };
        if let Some(err) = child.stderr.take() {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                // Progress lines redraw with a carriage return, which a text
                // box cannot do. Keep the substance, drop the animation.
                let t = line.trim();
                if t.contains('%') && t.starts_with(|c: char| c.is_ascii_digit()) {
                    continue;
                }
                if !t.is_empty() {
                    append(log, &format!("{line}\r\n"));
                }
            }
        }
        let _ = child.wait();
        append(log, "-- done --\r\n");
    });
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as usize;
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
                u.borrow().as_ref()
                    .map(|ui| (ui.list, ui.img, ui.dir, ui.log, ui.targets.clone()))
            });
            let Some((list, img, dir, log, targets)) = snap else { return LRESULT(0) };

            let image_path = text_of(img);
            let out_dir = text_of(dir);
            let target = selected(list, &targets);
            let need_target = |verb: &str| -> Option<String> {
                if target.is_none() {
                    append(log, &format!("[!] pick a disk or volume to {verb}
"));
                }
                target.clone()
            };
            let need = |v: &str, what: &str| -> bool {
                let ok = !v.trim().is_empty();
                if !ok {
                    append(log, &format!("[!] {what}
"));
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
                        run(log, vec!["scan".into(), t]);
                    }
                }
                ID_IMAGE => {
                    if let (Some(t), true) =
                        (need_target("image"), need(&image_path, "choose an image file to write"))
                    {
                        run(log, vec!["image".into(), t, image_path.clone()]);
                    }
                }
                ID_MOUNT => {
                    if need(&image_path, "choose the image file to mount") {
                        run(log, vec!["mount".into(), image_path.clone()]);
                    }
                }
                ID_UNDELETE => {
                    if let (Some(t), true) =
                        (need_target("recover from"), need(&out_dir, "choose a folder to recover into"))
                    {
                        run(log, vec!["undelete".into(), t, "--to".into(), out_dir.clone()]);
                    }
                }
                ID_CARVE => {
                    if let (Some(t), true) =
                        (need_target("carve"), need(&out_dir, "choose a folder to recover into"))
                    {
                        run(log, vec!["carve".into(), t, "--to".into(), out_dir.clone()]);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let snap = UI.with(|u| u.borrow().as_ref().map(|ui| (ui.list, ui.img, ui.dir, ui.log)));
            if let Some((list, img, dir, log)) = snap {
                let (w, h) = ((lp.0 & 0xFFFF) as i32, ((lp.0 >> 16) & 0xFFFF) as i32);
                unsafe {
                    let _ = MoveWindow(list, 10, 25, w - 20, 150, true);
                    let _ = MoveWindow(img, 150, 190, w - 250, 22, true);
                    let _ = MoveWindow(dir, 150, 220, w - 250, 22, true);
                    let _ = MoveWindow(log, 10, 300, w - 20, h - 310, true);
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
            WINDOW_EX_STYLE(0), class, w!("bulkhead"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT, CW_USEDEFAULT, 760, 620,
            None, None, Some(instance.into()), None,
        )?;

        control(w!("STATIC"), w!("Disks and volumes:"), 0, 10, 8, 300, 16, hwnd, 0);
        let list = control(w!("LISTBOX"),
                           PCWSTR::null(),
                           (WS_BORDER | WS_VSCROLL).0 | LBS_NOTIFY as u32,
                           10, 25, 720, 150, hwnd, ID_LIST);
        // Two paths, because they are two different things and one box asking
        // for "file/folder" could not say which a given button wanted.
        control(w!("STATIC"), w!("Image file (.vhdx):"), 0, 10, 193, 135, 16, hwnd, 0);
        let img = control(w!("EDIT"), PCWSTR::null(), (WS_BORDER | WS_TABSTOP).0,
                          150, 190, 510, 22, hwnd, ID_IMG);
        control(w!("BUTTON"), w!("Browse..."), WS_TABSTOP.0, 665, 189, 85, 24,
                hwnd, ID_BROWSE_IMG);

        control(w!("STATIC"), w!("Recover files into:"), 0, 10, 223, 135, 16, hwnd, 0);
        let dir = control(w!("EDIT"), PCWSTR::null(), (WS_BORDER | WS_TABSTOP).0,
                          150, 220, 510, 22, hwnd, ID_DIR);
        control(w!("BUTTON"), w!("Browse..."), WS_TABSTOP.0, 665, 219, 85, 24,
                hwnd, ID_BROWSE_DIR);

        let mut x = 10;
        for (id, label, width) in [
            (ID_REFRESH, w!("Refresh"), 80),
            (ID_SCAN, w!("Scan for lost partitions"), 150),
            (ID_IMAGE, w!("Disk -> image"), 110),
            (ID_MOUNT, w!("Mount image"), 100),
            (ID_UNDELETE, w!("Undelete files"), 110),
            (ID_CARVE, w!("Carve files"), 100),
        ] {
            control(w!("BUTTON"), label, WS_TABSTOP.0, x, 252, width, 26, hwnd, id);
            x += width + 6;
        }

        let log = control(w!("EDIT"), PCWSTR::null(),
                          (WS_BORDER | WS_VSCROLL).0
                              | ES_MULTILINE as u32 | ES_READONLY as u32 | ES_AUTOVSCROLL as u32,
                          10, 300, 740, 270, hwnd, ID_LOG);

        let targets = refresh(list);
        UI.with(|u| *u.borrow_mut() = Some(Ui { list, img, dir, log, targets }));

        append(log, "Pick a disk or volume above, then:\r\n");
        append(log, "  Disk -> image    copies it into the image file (a backup)\r\n");
        append(log, "  Mount image      attaches an image file as a drive to browse\r\n");
        append(log, "  Scan             looks for partitions whose table is lost\r\n");
        append(log, "  Undelete files   recovers deleted files into the folder\r\n");
        append(log, "  Carve files      last resort: pulls files out by signature,\r\n");
        append(log, "                   with no names, when no filesystem survives\r\n");
        append(log, "\r\nRead-only here. restore, part move and scan --rebuild write to\r\n");
        append(log, "disks and stay on the command line, where their confirmations are.\r\n");
        let admin = ps("([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)")
            .unwrap_or_default();
        if !admin.trim().eq_ignore_ascii_case("true") {
            append(log, "\r\n[!] Not elevated. Raw disk access will fail; restart as administrator.\r\n");
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
