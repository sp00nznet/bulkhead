# Changelog

All notable changes to bulkhead. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[SemVer](https://semver.org/).

## [Unreleased]

**If you built recovery media with v0.1.0, rebuild it.** The ISO's command-line
binary could not load in WinPE, the GUI was missing from it, and a disk restored
from it did not boot. All three are fixed below.

### Added
- `media` injects this machine's storage and network drivers into the WinPE
  image, so the recovery media can see the disks on the machine it was built
  for (b60b441).
- `media` ships `comctl32.dll`, so the GUI runs in WinPE (2dde714).
- A conformance harness that runs in CI: real ext2/ext4/XFS/HFS+ images made by
  each filesystem's own `mkfs`, read back and hashed, mounted through WinFsp,
  plus the image/restore/move/rebuild/undelete/carve smoke stages on throwaway
  disks. A check that used to pass and stops passing fails the build
  (`tools/conformance.ps1`).
- CHANGELOG, ROADMAP, CONTRIBUTING and SECURITY.

### Changed
- `undelete` puts recovered files back into their original directory tree
  instead of one flat folder (890edd8), and a re-run skips files it has already
  recovered (01c2e4a).
- `ls` shows what a directory holds (size and file count), not just its name
  (241382b).
- The README follows the house layout and claims only what has been run. ext2
  and ext3 are no longer listed as readable: they are refused, with a reason.

### Fixed
- The recovery binary died in WinPE's loader with `0xC0000135` before printing
  anything: it is now built with a static CRT and loads `comctl32.dll` lazily
  (cb5e34e).
- A restored disk didn't boot: Windows rewrote the restored partition table with
  new GUIDs. Restore now stops that (1c98b1c).
- ext4 read its incompatible-feature flags from the wrong offset (`0x64`
  instead of `0x60`) (f31e07e).
- `carve` refused filesystem image files that every other reader accepts
  (97036ff).
- CI had been failing on `cargo fmt --check` since 2026-09-07 (2eeebd1).

## [0.1.0] - 2026-08-31

First public release: `bulkhead.exe` (command line) and `bulkhead-gui.exe` (the
window).

### Added
- `image`: a VSS snapshot of a live volume, or a whole disk, to a dynamic VHDX.
  Only used clusters are copied. `--from` writes an incremental as a VHDX
  differencing disk.
- `mount` / `unmount` through `AttachVirtualDisk`, read-only by default.
- `restore`: an image back onto a disk, with the GPT relocated when the target is
  bigger.
- `part list` and `part move` (GPT). MBR disks are read, including logical
  partitions.
- `scan` / `scan --rebuild` / `undo`: find filesystems whose partition table is
  gone, write a new GPT, and put the old table back if needed.
- `undelete` from a surviving NTFS `$MFT`, and `carve` by file signature.
- `ls`, `cp` and `mount-fs` for ext4, XFS and HFS+.
- `identify`: RAID, LVM, ZFS, btrfs, bcachefs and other format signatures.
- `erase-info`, and `erase` by overwrite or ATA SANITIZE, with a certificate
  (`--cert`) written whether the erase passed or failed.
- `media`: bootable WinPE recovery ISO.
- `gui`: a native Win32 window over the same commands.

[Unreleased]: https://github.com/sp00nznet/bulkhead/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sp00nznet/bulkhead/releases/tag/v0.1.0
