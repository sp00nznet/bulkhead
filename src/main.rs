//! bulkhead -- the desktop build: a person, a broken machine, one disk.
//!
//! No service, no scheduler, no credentials. Unattended and multi-machine work
//! is `ballast`, in its own repo, which takes this crate as a library.
use bulkhead::*;

const USAGE: &str = "\
bulkhead -- block-level backup and recovery for Windows

  bulkhead image <VOL> <OUT.vhdx> [--from <PARENT.vhdx>] [--no-snapshot]
      Image a volume (e.g. C:) into a VHDX through a VSS snapshot.
      --from makes it an incremental: a VHDX differencing disk off PARENT.
      --no-snapshot images the volume directly (offline disks only).

  bulkhead mount <IMAGE.vhdx> [--rw]
      Attach the image as a drive. Read-only unless --rw.

  bulkhead unmount <IMAGE.vhdx>

  bulkhead restore <IMAGE.vhdx> <diskN> [--yes]
      ERASE a disk and write the image back over it. Asks first.
      A bigger target keeps its extra space: the GPT is extended to fit.

  bulkhead part list <diskN>
  bulkhead part move <diskN> <N> --to <OFFSET>   e.g. --to 1MB
      Slide a partition to a new offset. Windows cannot do this at all;
      it is also how you extend into free space that sits to the LEFT.

  bulkhead scan <diskN> [--rebuild] [--yes]
      Find filesystems on a disk whose partition table is lost, and
      optionally write a new table pointing at them. The scan is
      read-only; --rebuild saves the old table first.

  bulkhead undo <diskN> <TABLE.bin> [--yes]
      Put back a partition table saved by scan --rebuild.

  bulkhead undelete <VOL|diskN> --to <DIR> [--at <OFFSET>] [--limit <N>]
      Recover deleted files from an NTFS volume. Read-only on the source.
      --at gives the volume's byte offset when the target is a whole disk.

  bulkhead carve <VOL|diskN> --to <DIR> [--limit <N>]
      Last resort: pull files out by their signatures when no filesystem
      survives. No names, and fragmented files come back truncated.

  bulkhead ls <VOL|diskN> [PATH] [--at <OFFSET>]
  bulkhead cp <VOL|diskN> <PATH> --to <DIR> [--at <OFFSET>]
      Read ext2/3/4, XFS and HFS+ volumes, which Windows cannot. PATH is inside the
      filesystem; cp takes a file or a whole directory tree.

  bulkhead identify <VOL|diskN> [--at <OFFSET>]
      Say what a disk is and what set it belongs to -- RAID member, LVM
      or ZFS pool member, btrfs/bcachefs, SquashFS, UFS2, VMFS.

  bulkhead mount-fs <VOL|diskN|IMAGE> <X:> [--at <OFFSET>]
      Mount an ext2/3/4, XFS or HFS+ volume as a Windows drive, read-only.
      Needs WinFsp (winget install WinFsp.WinFsp).

  bulkhead erase-info <diskN>
      What erase commands a drive supports, and what is stopping one.
      Read-only.

  bulkhead erase <diskN> --method overwrite [--yes] [--cert <FILE>]
      ERASE a drive. Asks for its serial number first. Overwrite cannot
      reach blocks flash has remapped; a firmware sanitize can.
      --cert writes a certificate of erasure: .json for a machine,
      any other extension for a printable page. Written pass or fail.

  bulkhead gui
      A window over the read-only operations, for people who do not
      want a command line. Destructive commands stay here.

  bulkhead media <OUT.iso>
      Build bootable WinPE recovery media with bulkhead in it.
      Needs the Windows ADK and its separate WinPE add-on.

Needs an elevated prompt (raw volume access).";

/// Positional args, with `--flags` and their values removed.
fn positional<'a>(a: &[&'a str]) -> Vec<&'a str> {
    let mut v = Vec::new();
    let mut it = a.iter().copied();
    while let Some(x) = it.next() {
        match x {
            "--from" | "--to" | "--at" | "--limit" | "--method" | "--cert" => { it.next(); }
            _ if x.starts_with("--") => {}
            _ => v.push(x),
        }
    }
    v
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let flag = |f: &str| a.contains(&f);
    let opt = |f: &str| a.iter().position(|&x| x == f).and_then(|i| a.get(i + 1)).copied();
    let pos = positional(&a);

    let r = match pos.as_slice() {
        ["image", vol, out] => cmd_image(vol, out, !flag("--no-snapshot"), opt("--from")),
        ["mount", img] => cmd_mount(img, flag("--rw")),
        ["unmount", img] => cmd_unmount(img),
        ["restore", img, target] => cmd_restore(img, target, flag("--yes")),
        ["media", iso] => media::build(iso),
        ["gui"] => gui::run_gui(),
        ["identify", t] => cmd_identify(t, opt("--at").and_then(parse_size)),
        ["erase-info", t] => cmd_erase_info(t),
        ["erase", t] => cmd_erase(t, opt("--method"), flag("--yes"), opt("--cert")),
        ["mount-fs", t, mp] => {
            cmd_mount_fs(t, opt("--at").and_then(parse_size), mp, flag("--debug"))
        }
        ["ls", t] => cmd_ls(t, opt("--at").and_then(parse_size), "/"),
        ["ls", t, path] => cmd_ls(t, opt("--at").and_then(parse_size), path),
        ["cp", t, path] => match opt("--to") {
            Some(dir) => cmd_cp(t, opt("--at").and_then(parse_size), path, dir),
            None => Err("cp needs --to <DIR>".into()),
        },
        ["carve", t] => match opt("--to") {
            Some(dir) => cmd_carve(t, dir,
                                   opt("--limit").and_then(|l| l.parse().ok()).unwrap_or(5_000)),
            None => Err("carve needs --to <DIR>".into()),
        },
        ["undo", d, file] => disk_arg(d)
            .ok_or_else(|| format!("{d:?} is not a disk").into())
            .and_then(|n| cmd_undo(n, file, flag("--yes"))),
        ["undelete", t] => match opt("--to") {
            Some(dir) => cmd_undelete(t, opt("--at").and_then(parse_size), dir,
                                      opt("--limit").and_then(|l| l.parse().ok()).unwrap_or(10_000)),
            None => Err("undelete needs --to <DIR>".into()),
        },
        ["scan", d] => disk_arg(d)
            .ok_or_else(|| format!("{d:?} is not a disk").into())
            .and_then(|n| cmd_scan(n, flag("--rebuild"), flag("--yes"))),
        ["part", "list", d] => disk_arg(d)
            .ok_or_else(|| format!("{d:?} is not a disk").into())
            .and_then(cmd_part_list),
        ["part", "move", d, n] => match (disk_arg(d), n.parse(), opt("--to").and_then(parse_size)) {
            (Some(d), Ok(n), Some(to)) => cmd_part_move(d, n, to, flag("--yes")),
            (_, _, None) => Err("part move needs --to <OFFSET>".into()),
            _ => Err(format!("bad disk or partition number: {d:?} {n:?}").into()),
        },
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
    fn args() {
        assert_eq!(positional(&["image", "C:", "o.vhdx"]), ["image", "C:", "o.vhdx"]);
        // options swallow their value; bare flags vanish
        assert_eq!(
            positional(&["image", "C:", "o.vhdx", "--from", "p.vhdx", "--no-snapshot"]),
            ["image", "C:", "o.vhdx"]
        );
        // a flag wedged between positionals must not eat one
        assert_eq!(positional(&["mount", "--rw", "o.vhdx"]), ["mount", "o.vhdx"]);
        // trailing --from with no value must not panic
        assert_eq!(positional(&["mount", "o.vhdx", "--from"]), ["mount", "o.vhdx"]);
    }
}
