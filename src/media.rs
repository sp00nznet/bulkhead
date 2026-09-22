//! Build bootable WinPE recovery media.
//!
//! The ADK already knows how to make WinPE (`copype`, `MakeWinPEMedia`). What
//! it does not do is put bulkhead in it, add the optional components our
//! partitioning path needs -- base WinPE has no PowerShell at all -- or carry
//! the driver for the controller the disk is actually behind.
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

/// Driver classes harvested from the build machine into the media.
///
/// WinPE ships an inbox driver set that covers commodity AHCI and NVMe and
/// stops there. The controller it does not know about is the one holding the
/// disk you came to recover, and the failure mode is a recovery prompt that
/// lists no disks at all.
///
/// ponytail: a class filter, not a boot-criticality query. `Get-WindowsDriver`
/// only reports `BootCritical` on the per-driver advanced object, which is a
/// DISM round trip each; these three classes are what actually strands a boot
/// -- the disk controller, the legacy ATA controller, and the NIC for pulling
/// an image off a NAS. Widen the list if a machine turns up whose boot device
/// hides somewhere else.
const DRIVER_CLASSES: &[&str] = &["SCSIAdapter", "HDC", "Net"];

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
echo   Or drive it from a window instead:  bulkhead gui
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

/// Put `comctl32.dll` in the image, because WinPE has no common controls.
///
/// The GUI calls `InitCommonControlsEx` for the progress bar, and without this
/// the window cannot come up at all -- the binary delay-loads comctl32 (see
/// `.cargo/config.toml`), so the CLI is fine either way and only `bulkhead gui`
/// needs the file. bulkhead has no application manifest, so it binds to the
/// plain v5 comctl32 in System32 rather than a WinSxS v6 assembly, and a
/// straight copy of the host's is the whole job.
///
/// Missing is a warning, not an error: the media is still worth building for
/// the command line.
fn install_comctl32(sys32: &std::path::Path) -> Res<()> {
    let src = std::path::Path::new(&std::env::var("SystemRoot").unwrap_or(r"C:\Windows".into()))
        .join("System32")
        .join("comctl32.dll");
    if !src.is_file() {
        eprintln!(
            "[!] {} not found -- `bulkhead gui` will not run on this media",
            src.display()
        );
        return Ok(());
    }
    eprintln!("[*] installing comctl32.dll (WinPE has none, and the GUI needs it)");
    std::fs::copy(&src, sys32.join("comctl32.dll"))?;
    Ok(())
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

pub fn build(out_iso: &str, extra: Option<&str>) -> Res<()> {
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
        // Speculative: only a previous run that died mid-DISM leaves an image
        // mounted here. With nothing mounted DISM answers "Error: 50, The
        // request is not supported" and prints a banner that reads like a
        // failure, so take .output() and swallow it -- the result was always
        // ignored, it just was not quiet about it.
        let _ = Command::new("dism")
            .args([
                "/Unmount-Image",
                &format!("/MountDir:{}", mount.display()),
                "/Discard",
            ])
            .output();
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
    let r = populate(&exe, &mount, &ocs, extra);
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

/// The PowerShell that copies this machine's matching driver packages into
/// `dir`, and prints how many it found.
///
/// Split out from [`harvest_drivers`] so a test can read it without a driver
/// store, an elevated prompt or ten minutes of DISM.
fn harvest_script(dir: &Path) -> String {
    let classes = DRIVER_CLASSES
        .iter()
        .map(|c| format!("'{c}'"))
        .collect::<Vec<_>>()
        .join(",");
    // Copy the whole package directory, not the .inf: the .sys and the .cat
    // live beside it in the DriverStore and DISM needs a tree it can walk.
    //
    // The count comes from the collected array rather than a `$i++` inside the
    // loop -- ForEach-Object's block is a child scope, so the increment would
    // write to a copy and this would always report zero.
    format!(
        "$found = @(Get-WindowsDriver -Online | Where-Object {{ $_.ClassName -in {classes} }});
         $found | ForEach-Object {{
            $p = Split-Path $_.OriginalFileName;
            Copy-Item -Recurse -Force -LiteralPath $p \
                      -Destination (Join-Path '{}' (Split-Path $p -Leaf)) }};
         $found.Count",
        dir.display()
    )
}

/// Copy this machine's third-party storage and network drivers into `dir`.
///
/// `-Online` without `-All` is already only the out-of-box drivers, which is
/// the right set: the inbox ones are what WinPE has too.
fn harvest_drivers(dir: &Path) -> Res<u32> {
    let out = ps(&harvest_script(dir))?;
    out.trim()
        .parse()
        .map_err(|_| format!("expected a driver count, got {out:?}").into())
}

/// Add every driver package under `src` to the mounted image.
fn add_drivers(mount: &Path, src: &Path) -> Res<()> {
    sh(
        &format!("injecting drivers from {}", src.display()),
        &format!(
            "dism /Image:\"{}\" /Add-Driver /Driver:\"{}\" /Recurse",
            mount.display(),
            src.display()
        ),
    )
}

/// Harvest and inject, best-effort.
///
/// Deliberately never fatal. This runs several minutes into a DISM build and
/// the media is still worth having without it -- WinPE's inbox set covers
/// commodity AHCI and NVMe, which is most machines. Say plainly what is
/// missing rather than throwing the build away.
fn install_drivers(mount: &Path) {
    let store = std::env::temp_dir().join("bulkhead-winpe-drivers");
    let _ = std::fs::remove_dir_all(&store);
    if let Err(e) = std::fs::create_dir_all(&store) {
        eprintln!("[!] cannot stage drivers in {}: {e}", store.display());
        return;
    }
    eprintln!("[*] harvesting this machine's storage and network drivers");
    match harvest_drivers(&store) {
        Ok(0) => eprintln!(
            "[!] no third-party {} drivers here -- media carries WinPE's inbox set only",
            DRIVER_CLASSES.join("/")
        ),
        Ok(n) => {
            eprintln!("[*] {n} driver package(s) to inject");
            if let Err(e) = add_drivers(mount, &store) {
                eprintln!("[!] {e}\n    media will boot, but only for controllers WinPE knows");
            }
        }
        Err(e) => eprintln!("[!] harvest failed: {e}\n    building without added drivers"),
    }
    let _ = std::fs::remove_dir_all(&store);
}

fn populate(exe: &Path, mount: &Path, ocs: &Path, extra: Option<&str>) -> Res<()> {
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

    install_drivers(mount);

    // An explicit --drivers is a request, not a convenience: if it fails, the
    // user asked for something they did not get, and almost certainly asked
    // because their controller is the one WinPE cannot see.
    if let Some(d) = extra {
        let d = Path::new(d);
        need(d, "--drivers directory")?;
        add_drivers(mount, d)?;
    }

    let sys32 = mount.join("Windows").join("System32");
    eprintln!("[*] installing bulkhead.exe");
    std::fs::copy(exe, sys32.join("bulkhead.exe"))?;
    install_comctl32(&sys32)?;
    std::fs::write(sys32.join("startnet.cmd"), STARTNET.replace('\n', "\r\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The harvest query is the one piece of real logic here that does not
    /// need an ADK, an elevated prompt or ten minutes to get wrong.
    #[test]
    fn harvest_script_quotes_classes_and_destination() {
        let s = harvest_script(Path::new(r"C:\Temp\drv"));
        // Each class single-quoted inside the -in list, or the filter matches
        // nothing and the media silently ships with no added drivers.
        for c in DRIVER_CLASSES {
            assert!(s.contains(&format!("'{c}'")), "{c} not quoted in: {s}");
        }
        assert!(s.contains("-in 'SCSIAdapter','HDC','Net'"), "{s}");
        assert!(
            s.contains(r"'C:\Temp\drv'"),
            "destination not quoted in: {s}"
        );
        // Counting the array, not incrementing inside ForEach-Object.
        assert!(s.contains("$found.Count"), "{s}");
        assert!(!s.contains("$i++"), "{s}");
    }
}
