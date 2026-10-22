# Architecture

One Rust library, two executables, and the Windows components they lean on.

```
 bulkhead-gui.exe ──spawns──> bulkhead.exe ──> library (src/lib.rs + modules)
   window, log, prompts        CLI (src/main.rs)        │
                                                         ├── raw disk / volume I/O (DeviceIoControl)
                                                         ├── VHDX: virtdisk.dll (CreateVirtualDisk, AttachVirtualDisk)
                                                         ├── VSS and new partitions: PowerShell (src/util.rs)
                                                         ├── ATA / NVMe pass-through (src/sanitize.rs, src/erase.rs)
                                                         └── WinFsp, loaded at runtime (src/winfsp.rs)
```

## The parts

| Part | Owns |
|---|---|
| `src/lib.rs` | Every command's logic: imaging, restore, partitions, undelete, erase, reading foreign filesystems. Opens disks, volumes and image files the same way for all readers (`open_target`) |
| `src/main.rs` → `bulkhead.exe` | Argument parsing and the confirmation prompts. Console subsystem |
| `src/gui.rs` → `bulkhead-gui.exe` (or `bulkhead gui`) | The window. It never touches a disk: every button runs `bulkhead.exe` from the same folder as a child process and shows its output. See [The window](gui.md) |
| `src/media.rs` | Builds the WinPE ISO with the ADK. Copies in the CLI binary (always `bulkhead.exe`, even when run from the GUI build), `comctl32.dll` so the window works, and this machine's storage and network drivers |
| `ntfs.rs`, `ext4.rs`, `xfs.rs`, `hfs.rs` | Read-only filesystem readers, written from the published on-disk specs |
| `gpt.rs`, `mbr.rs`, `scan.rs` | Partition tables: read, relocate, and rebuild from the filesystems found on a disk |
| `erase.rs`, `sanitize.rs`, `cert.rs` | What a drive can do to erase itself, doing it, and the certificate |

## Why the window is a separate process

The GUI passes arguments and the user's typed answers to the CLI's own
prompts (the drive serial for `erase`, `YES` for `restore`). So the
confirmation logic exists once, in the CLI, and the window can't skip it: it
has no `--yes` to give. The cost is that **Cancel** kills the child, which
skips its cleanup and can leave an image attached or a VSS snapshot behind.

## Why VHDX

The format carries most of the feature list:

| The job | How bulkhead does it |
|---|---|
| Block-level snapshot of a live volume | `Win32_ShadowCopy`, then read the shadow device raw |
| Whole-disk image | GPT copied verbatim; the VHDX is the source disk's size |
| Incremental | A VHDX **differencing disk**. bulkhead finds the changed blocks by reading and comparing the parent, not by tracking writes ([design notes](design.md)) |
| Mount an image as a drive | `AttachVirtualDisk`: it shows up in Explorer |
| Bootable recovery media | WinPE from the ADK, plus a `startnet.cmd` |
| Restore to bare metal | Write the image back to a disk, from the recovery media |

A VHDX opens in Explorer, Hyper-V and `Mount-DiskImage`, with or without
bulkhead installed. A proprietary image format is how a backup tool locks
people in.

## Who runs it

bulkhead is the desktop build: a person at a broken machine with one disk in
front of them. No service, no scheduler, nothing that wants a credential.
Unattended, scheduled work belongs in a separate program built on the same
library. A scheduler dependency that creeps into a desktop tool doesn't
announce itself. The filesystem readers live in the library because "get
three files off this Linux disk" is also something file-level restore needs.

## Outside dependencies

- **The `windows` crate** is the only Rust dependency.
- **PowerShell** drives VSS and partition creation (`src/util.rs`). It's on
  every live Windows; the recovery media adds it to WinPE. Its ceiling is in
  [design notes](design.md).
- **WinFsp** is loaded with `LoadLibraryW` only by `mount-fs`, so every other
  command works without it and building needs no SDK. WinFsp is GPLv3 with an
  exception for free and open-source software; bulkhead meets its conditions,
  including carrying its notice ([Reading ext4, XFS and HFS+](filesystems.md)).

---

[< back to the README](../README.md)
