//! Build bootable WinPE recovery media.
//!
//! The ADK already knows how to make WinPE (`copype`, `MakeWinPEMedia`). What
//! it does not do is put bulkhead in it, or add the optional components our
//! partitioning path needs -- base WinPE has no PowerShell at all.
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::{Res, ps};

/// Base WinPE cannot run `Initialize-Disk` / `New-Partition` / `Set-Disk`,
/// which is how `image` lays out its target. These are the documented
/// dependency chain for the Storage cmdlets, and the order matters.
const COMPONENTS: &[&str] = &[
    "WinPE-WMI",
    "WinPE-NetFX",
    "WinPE-Scripting",
    "WinPE-PowerShell",
    "WinPE-StorageWMI",
];

const STARTNET: &str = "\
@echo off
wpeinit
echo.
echo   bulkhead recovery media
echo.
echo   VSS does not exist here, so imaging is always --no-snapshot.
echo   That is fine: nothing in WinPE is writing to the disk you are imaging.
echo.
echo     bulkhead image D: E:\\backup.vhdx --no-snapshot
echo     bulkhead mount E:\\backup.vhdx
echo.
";

/// Run a batch snippet.
///
/// Via a .bat file rather than `cmd /c "..."` on purpose: Rust escapes quotes
/// in arguments the way the C runtime parses them, and cmd.exe does not use
/// that convention, so a quoted path arrives with literal backslash-quotes and
/// cmd reports the whole thing as an unrecognised command. A file has no
/// quoting layer to get wrong.
fn sh(what: &str, script: &str) -> Res<()> {
    eprintln!("[*] {what}");
    let bat = std::env::temp_dir().join(format!("bulkhead-{}.bat", std::process::id()));
    std::fs::write(
        &bat,
        format!("@echo off\r\n{}\r\n", script.replace('\n', "\r\n")),
    )?;
    // Inherited stdio on purpose -- DISM runs for minutes and its progress
    // meter is the only sign it is alive.
    let st = Command::new("cmd").arg("/c").arg(&bat).status()?;
    let _ = std::fs::remove_file(&bat);
    if !st.success() {
        return Err(format!("{what} failed ({st})").into());
    }
    Ok(())
}

fn need(p: &Path, what: &str) -> Res<()> {
    if p.exists() {
        Ok(())
    } else {
        Err(format!("{what} not found at {}", p.display()).into())
    }
}

/// Where the ADK might be.
///
/// `KitsRoot10` is not one value: adksetup.exe is 32-bit and registers under
/// WOW6432Node, while the Windows SDK registers in the 64-bit view -- so on a
/// machine with both, the obvious lookup returns the SDK's path and the ADK
/// looks missing. Collect every candidate and let the caller pick the one that
/// actually holds the tools.
fn adk_roots() -> Res<Vec<PathBuf>> {
    let reg = ps(
        "@('HKLM:\\SOFTWARE\\Microsoft\\Windows Kits\\Installed Roots', \
            'HKLM:\\SOFTWARE\\WOW6432Node\\Microsoft\\Windows Kits\\Installed Roots') | \
         ForEach-Object { (Get-ItemProperty $_ -ErrorAction SilentlyContinue).KitsRoot10 }",
    )?;
    let mut v: Vec<PathBuf> = reg
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Ok(pf) = std::env::var(var) {
            v.push(PathBuf::from(pf).join("Windows Kits").join("10"));
        }
    }
    v.dedup();
    Ok(v)
}

/// The `bulkhead.exe` to install into the media.
///
/// Normally this is the running binary, but `media` can be invoked from
/// `bulkhead-gui.exe`, which is useless on the recovery prompt. In that case
/// take its sibling `bulkhead.exe`, which `cargo build` puts right next to it.
fn cli_exe() -> Res<std::path::PathBuf> {
    let me = std::env::current_exe()?;
    let is_gui = me
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("bulkhead-gui"));
    if !is_gui {
        return Ok(me);
    }
    let cli = me.with_file_name("bulkhead.exe");
    if cli.is_file() {
        eprintln!(
            "[*] building from the GUI binary; installing {} instead",
            cli.display()
        );
        return Ok(cli);
    }
    Err(format!(
        "run `media` from bulkhead.exe, not bulkhead-gui.exe.
             The GUI binary has no console, so on the recovery prompt every
             command would exit without printing anything. Expected to find
             {} beside it.",
        cli.display()
    )
    .into())
}

pub fn build(out_iso: &str) -> Res<()> {
    // Checked up front because the first thing that needs it is a DISM
    // preflight whose failure is neither fatal nor obviously about privilege.
    let admin = ps(
        "([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent())\
         .IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
    )?;
    if !admin.trim().eq_ignore_ascii_case("true") {
        return Err("building media needs an elevated prompt (DISM services the image)".into());
    }

    let roots = adk_roots()?;
    let adk = roots
        .iter()
        .map(|r| r.join("Assessment and Deployment Kit"))
        .find(|a| a.join("Deployment Tools").join("DandISetEnv.bat").exists())
        .ok_or_else(|| {
            let tried: Vec<String> = roots
                .iter()
                .map(|r| format!("\n      {}", r.display()))
                .collect();
            format!(
                "Windows ADK not installed.\n    \
                 Get it from https://aka.ms/adk and tick 'Deployment Tools'.\n    \
                 Looked in:{}",
                tried.concat()
            )
        })?;
    eprintln!("[*] ADK at {}", adk.display());
    let dandi = adk.join("Deployment Tools").join("DandISetEnv.bat");
    let ocs = adk
        .join("Windows Preinstallation Environment")
        .join("amd64")
        .join("WinPE_OCs");
    // The WinPE add-on is a separate download, so the ADK can be present
    // without it. Say which of the two is missing.
    if !ocs.exists() {
        return Err(format!(
            "WinPE add-on not installed ({} is missing).\n    \
             It is a separate download from the ADK, same page: https://aka.ms/adk",
            ocs.display()
        )
        .into());
    }

    // The media must carry the CLI binary. bulkhead-gui is built
    // `windows_subsystem = "windows"`, so a copy of it in System32 runs,
    // detaches from the console and exits without printing a byte -- the
    // recovery prompt then looks fine and every command silently does
    // nothing. Building the ISO from the GUI binary used to produce exactly
    // that, because this was `current_exe()` and nothing checked.
    let exe = cli_exe()?;
    let work = std::env::temp_dir().join("bulkhead-winpe");
    let mount = work.join("mount");
    let wim = work.join("media").join("sources").join("boot.wim");

    // copype refuses to write into an existing directory, and a previous run
    // that died mid-DISM leaves the image registered as mounted.
    let _ = Command::new("dism").args(["/Cleanup-Wim"]).status();
    if work.exists() {
        eprintln!("[*] clearing {}", work.display());
        let _ = Command::new("dism")
            .args([
                "/Unmount-Image",
                &format!("/MountDir:{}", mount.display()),
                "/Discard",
            ])
            .status();
        std::fs::remove_dir_all(&work)?;
    }

    let env = format!("call \"{}\"", dandi.display());
    sh(
        "copype amd64",
        &format!("{env} && call copype amd64 \"{}\"", work.display()),
    )?;
    need(&wim, "boot.wim (copype did not produce one)")?;

    sh(
        "mounting boot.wim",
        &format!(
            "dism /Mount-Image /ImageFile:\"{}\" /Index:1 /MountDir:\"{}\"",
            wim.display(),
            mount.display()
        ),
    )?;

    // From here on, unmount before returning any error -- leaving an image
    // mounted wedges the next run and needs a manual /Cleanup-Wim.
    let r = populate(&exe, &mount, &ocs);
    let unmount = sh(
        "committing boot.wim",
        &format!(
            "dism /Unmount-Image /MountDir:\"{}\" /Commit",
            mount.display()
        ),
    );
    r?;
    unmount?;

    sh(
        "building ISO",
        &format!(
            "{env} && call MakeWinPEMedia /ISO /f \"{}\" \"{out_iso}\"",
            work.display()
        ),
    )?;

    eprintln!("[+] {out_iso}");
    eprintln!(
        "    burn it, or:  MakeWinPEMedia /UFD \"{}\" F:",
        work.display()
    );
    Ok(())
}

fn populate(exe: &Path, mount: &Path, ocs: &Path) -> Res<()> {
    for c in COMPONENTS {
        let cab = ocs.join(format!("{c}.cab"));
        need(&cab, c)?;
        sh(
            &format!("adding {c}"),
            &format!(
                "dism /Image:\"{}\" /Add-Package /PackagePath:\"{}\"",
                mount.display(),
                cab.display()
            ),
        )?;
        // Language pack is separate and must follow its component.
        let lang = ocs.join("en-us").join(format!("{c}_en-us.cab"));
        if lang.exists() {
            sh(
                &format!("adding {c} (en-us)"),
                &format!(
                    "dism /Image:\"{}\" /Add-Package /PackagePath:\"{}\"",
                    mount.display(),
                    lang.display()
                ),
            )?;
        }
    }

    let sys32 = mount.join("Windows").join("System32");
    eprintln!("[*] installing bulkhead.exe");
    std::fs::copy(exe, sys32.join("bulkhead.exe"))?;
    std::fs::write(sys32.join("startnet.cmd"), STARTNET.replace('\n', "\r\n"))?;
    Ok(())
}
