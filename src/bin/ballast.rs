//! ballast -- the server build: unattended, scheduled, many machines.
//!
//! Same library as `bulkhead`, different operator. bulkhead is driven by a
//! person standing at one broken machine; ballast is driven by a scheduler and
//! answers to a console.
//!
//! Today it does the one thing that needs no new subsystem: keep a backup chain
//! for a volume in a directory, picking full-versus-incremental itself so a
//! scheduled task can call the same line every night.
use std::path::{Path, PathBuf};

use bulkhead::cmd_image;
use bulkhead::util::Res;

const USAGE: &str = "\
ballast -- unattended backup, on top of the bulkhead core

  ballast backup <VOL> <DIR> [--full]
      Back <VOL> up into <DIR>, continuing the chain already there.
      An empty directory gets a full; otherwise this is an incremental
      off the newest image. --full starts a new chain.

      Meant to be the whole of a scheduled task's command line.

Needs an elevated prompt (raw volume access).";

/// Chain members are `<label>-NNNN.vhdx`, so the order is in the name and does
/// not depend on a timestamp, a database, or the order the directory lists in.
fn seq_of(name: &str, label: &str) -> Option<u32> {
    name.strip_prefix(label)?
        .strip_prefix('-')?
        .strip_suffix(".vhdx")?
        .parse()
        .ok()
}

/// The newest chain member and the number the next one gets.
fn newest<'a>(names: &[&'a str], label: &str) -> (Option<&'a str>, u32) {
    match names.iter().filter_map(|n| seq_of(n, label).map(|s| (s, *n))).max() {
        Some((s, n)) => (Some(n), s + 1),
        None => (None, 0),
    }
}

/// `C:` -> `c`. Whatever the volume is called, the chain needs a filename-safe
/// stem that is stable across runs.
fn label_for(volume: &str) -> String {
    let s: String = volume.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if s.is_empty() { "vol".into() } else { s.to_ascii_lowercase() }
}

fn backup(volume: &str, dir: &str, force_full: bool) -> Res<()> {
    let dir = Path::new(dir);
    std::fs::create_dir_all(dir)?;

    let label = label_for(volume);
    let entries: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
    let (parent, next) = newest(&refs, &label);
    let parent = if force_full { None } else { parent };

    let out = dir.join(format!("{label}-{next:04}.vhdx"));
    let out = out.to_string_lossy().into_owned();
    let parent: Option<PathBuf> = parent.map(|p| dir.join(p));
    let parent = parent.as_ref().map(|p| p.to_string_lossy().into_owned());

    match &parent {
        Some(p) => eprintln!("[*] incremental off {p}"),
        None => eprintln!("[*] full -- new chain"),
    }
    cmd_image(volume, &out, true, parent.as_deref())
}

// ponytail: no retention. A differencing child depends on its parent forever,
// so pruning is a *merge* (MergeVirtualDisk / Merge-VHD), not a delete, and
// getting it wrong silently orphans every descendant -- which stays invisible
// until the restore that needed it. Chains grow without bound until that lands.
// Ceiling: fine for a nightly chain someone re-fulls by hand, wrong for a fleet.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let pos: Vec<&str> = a.iter().copied().filter(|x| !x.starts_with("--")).collect();

    let r = match pos.as_slice() {
        ["backup", vol, dir] => backup(vol, dir, a.contains(&"--full")),
        _ => { eprintln!("{USAGE}"); std::process::exit(2); }
    };
    if let Err(e) = r {
        eprintln!("[!] {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_order() {
        // empty directory starts at 0 with no parent
        assert_eq!(newest(&[], "c"), (None, 0));

        // the newest is by number, not by directory order
        let n = ["c-0000.vhdx", "c-0002.vhdx", "c-0001.vhdx"];
        assert_eq!(newest(&n, "c"), (Some("c-0002.vhdx"), 3));

        // ten members must not sort as strings -- 0010 beats 0009
        let n = ["c-0009.vhdx", "c-0010.vhdx"];
        assert_eq!(newest(&n, "c"), (Some("c-0010.vhdx"), 11));

        // another volume's chain in the same directory is not ours
        let n = ["d-0007.vhdx", "c-0001.vhdx"];
        assert_eq!(newest(&n, "c"), (Some("c-0001.vhdx"), 2));

        // a prefix that merely starts the same is not a match
        assert_eq!(newest(&["cd-0003.vhdx"], "c"), (None, 0));

        // junk in the directory is ignored, not parsed
        let n = ["notes.txt", "c-.vhdx", "c-0001.vhdx.bak", "c-0004.vhdx"];
        assert_eq!(newest(&n, "c"), (Some("c-0004.vhdx"), 5));
    }

    #[test]
    fn labels() {
        assert_eq!(label_for("C:"), "c");
        assert_eq!(label_for(r"\\.\PhysicalDrive2"), "physicaldrive2");
        assert_eq!(label_for("::"), "vol");
    }
}
