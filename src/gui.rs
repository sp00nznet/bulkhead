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
const ID_PATH: usize = 101;
const ID_LOG: usize = 102;
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
    path: HWND,
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
                u.borrow().as_ref().map(|ui| (ui.list, ui.path, ui.log, ui.targets.clone()))
            });
            let Some((list, path, log, targets)) = snap else { return LRESULT(0) };

            let out = text_of(path);
            let target = selected(list, &targets);
            let need_target = |verb: &str| -> Option<String> {
                if target.is_none() {
                    append(log, &format!("[!] pick a disk or volume to {verb}
"));
                }
                target.clone()
            };
            let need_path = |what: &str| -> bool {
                let ok = !out.trim().is_empty();
                if !ok {
                    append(log, &format!("[!] fill in the output {what} first
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
                ID_SCAN => {
                    if let Some(t) = need_target("scan") {
                        run(log, vec!["scan".into(), t]);
                    }
                }
                ID_IMAGE => {
                    if let (Some(t), true) = (need_target("image"), need_path("file (.vhdx)")) {
                        run(log, vec!["image".into(), t, out.clone()]);
                    }
                }
                ID_UNDELETE => {
                    if let (Some(t), true) = (need_target("recover from"), need_path("folder")) {
                        run(log, vec!["undelete".into(), t, "--to".into(), out.clone()]);
                    }
                }
                ID_CARVE => {
                    if let (Some(t), true) = (need_target("carve"), need_path("folder")) {
                        run(log, vec!["carve".into(), t, "--to".into(), out.clone()]);
                    }
                }
                ID_MOUNT => {
                    if need_path("file (.vhdx)") {
                        run(log, vec!["mount".into(), out.clone()]);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let snap = UI.with(|u| u.borrow().as_ref().map(|ui| (ui.list, ui.path, ui.log)));
            if let Some((list, path, log)) = snap {
                let (w, h) = ((lp.0 & 0xFFFF) as i32, ((lp.0 >> 16) & 0xFFFF) as i32);
                unsafe {
                    let _ = MoveWindow(list, 10, 25, w - 20, 150, true);
                    let _ = MoveWindow(path, 100, 190, w - 110, 22, true);
                    let _ = MoveWindow(log, 10, 260, w - 20, h - 270, true);
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
        control(w!("STATIC"), w!("Output file/folder:"), 0, 10, 193, 90, 16, hwnd, 0);
        let path = control(w!("EDIT"), PCWSTR::null(), (WS_BORDER | WS_TABSTOP).0,
                           100, 190, 630, 22, hwnd, ID_PATH);

        let mut x = 10;
        for (id, label) in [
            (ID_REFRESH, w!("Refresh")),
            (ID_SCAN, w!("Scan for lost partitions")),
            (ID_IMAGE, w!("Image")),
            (ID_UNDELETE, w!("Undelete")),
            (ID_CARVE, w!("Carve")),
            (ID_MOUNT, w!("Mount image")),
        ] {
            let width = if id == ID_SCAN { 160 } else { 100 };
            control(w!("BUTTON"), label, WS_TABSTOP.0, x, 222, width, 26, hwnd, id);
            x += width + 8;
        }

        let log = control(w!("EDIT"), PCWSTR::null(),
                          (WS_BORDER | WS_VSCROLL).0
                              | ES_MULTILINE as u32 | ES_READONLY as u32 | ES_AUTOVSCROLL as u32,
                          10, 260, 720, 300, hwnd, ID_LOG);

        let targets = refresh(list);
        UI.with(|u| *u.borrow_mut() = Some(Ui { list, path, log, targets }));

        append(log, "bulkhead -- pick a disk or volume, set an output path, press a button.\r\n");
        append(log, "Read-only operations only. restore, part move and scan --rebuild\r\n");
        append(log, "stay on the command line, where their confirmations are.\r\n");
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
