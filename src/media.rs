//! Build bootable WinPE recovery media.
//!
//! The ADK already knows how to make WinPE (`copype`, `MakeWinPEMedia`). What
//! it does not do is put bulkhead in it, or add the optional components our
//! partitioning path needs -- base WinPE has no PowerShell at all.
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::{ps, Res};

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

fn sh(what: &str, script: &str) -> Res<()> {
    eprintln!("[*] {what}");
    // Inherited stdio on purpose -- DISM runs for minutes and its progress
    // meter is the only sign it is alive.
    let st = Command::new("cmd").args(["/c", script]).status()?;
    if !st.success() {
        return Err(format!("{what} failed ({st})").into());
    }
    Ok(())
}

fn need(p: &Path, what: &str) -> Res<()> {
    if p.exists() { Ok(()) } else { Err(format!("{what} not found at {}", p.display()).into()) }
}

pub fn build(out_iso: &str) -> Res<()> {
    let kits = ps(
        r"(Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots' -ErrorAction SilentlyContinue).KitsRoot10",
    )?;
    if kits.is_empty() {
        return Err("Windows ADK not installed. Install the ADK *and* the separate \
                    'Windows PE add-on' from https://aka.ms/adk -- WinPE has shipped \
                    as its own download since ADK 1809."
            .into());
    }

    let adk = PathBuf::from(kits.trim()).join("Assessment and Deployment Kit");
    let dandi = adk.join("Deployment Tools").join("DandISetEnv.bat");
    let ocs = adk
        .join("Windows Preinstallation Environment")
        .join("amd64")
        .join("WinPE_OCs");
    // KitsRoot10 is shared with the Windows SDK, so it can exist while neither
    // of these does. Say which of the two downloads is missing.
    if !dandi.exists() {
        return Err(format!(
            "Windows ADK not installed ({} is missing).\n    \
             Get it from https://aka.ms/adk and tick 'Deployment Tools'.",
            dandi.display()
        ).into());
    }
    if !ocs.exists() {
        return Err(format!(
            "WinPE add-on not installed ({} is missing).\n    \
             It is a separate download from the ADK, same page: https://aka.ms/adk",
            ocs.display()
        ).into());
    }

    let exe = std::env::current_exe()?;
    let work = std::env::temp_dir().join("bulkhead-winpe");
    let mount = work.join("mount");
    let wim = work.join("media").join("sources").join("boot.wim");

    // copype refuses to write into an existing directory, and a previous run
    // that died mid-DISM leaves the image registered as mounted.
    let _ = Command::new("dism").args(["/Cleanup-Wim"]).status();
    if work.exists() {
        eprintln!("[*] clearing {}", work.display());
        let _ = Command::new("dism")
            .args(["/Unmount-Image", &format!("/MountDir:{}", mount.display()), "/Discard"])
            .status();
        std::fs::remove_dir_all(&work)?;
    }

    let env = format!("call \"{}\"", dandi.display());
    sh("copype amd64", &format!("{env} && call copype amd64 \"{}\"", work.display()))?;
    need(&wim, "boot.wim (copype did not produce one)")?;

    sh(
        "mounting boot.wim",
        &format!(
            "dism /Mount-Image /ImageFile:\"{}\" /Index:1 /MountDir:\"{}\"",
            wim.display(), mount.display()
        ),
    )?;

    // From here on, unmount before returning any error -- leaving an image
    // mounted wedges the next run and needs a manual /Cleanup-Wim.
    let r = populate(&exe, &mount, &ocs);
    let unmount = sh(
        "committing boot.wim",
        &format!("dism /Unmount-Image /MountDir:\"{}\" /Commit", mount.display()),
    );
    r?;
    unmount?;

    sh(
        "building ISO",
        &format!("{env} && call MakeWinPEMedia /ISO /f \"{}\" \"{out_iso}\"", work.display()),
    )?;

    eprintln!("[+] {out_iso}");
    eprintln!("    burn it, or:  MakeWinPEMedia /UFD \"{}\" F:", work.display());
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
                mount.display(), cab.display()
            ),
        )?;
        // Language pack is separate and must follow its component.
        let lang = ocs.join("en-us").join(format!("{c}_en-us.cab"));
        if lang.exists() {
            sh(
                &format!("adding {c} (en-us)"),
                &format!(
                    "dism /Image:\"{}\" /Add-Package /PackagePath:\"{}\"",
                    mount.display(), lang.display()
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
