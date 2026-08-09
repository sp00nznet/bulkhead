# bulkhead

Free, open block-level backup and recovery for Windows.

Macrium killed Reflect Free. Acronis went subscription. EaseUS, AOMEI and
MiniTool ship nagware that lets you *make* a backup and then paywalls the
restore. Paragon charges ~$40 per filesystem to read ext4 on Windows. Blancco
charges per drive for an ATA command and a PDF.

None of this is hard science. Most of it is already implemented **inside
Windows** — VSS, VHDX differencing disks, `AttachVirtualDisk`, ATA/NVMe
sanitize commands. The paid tools are charging for the integration. bulkhead is
that integration, given away.

> ⚠️ Pre-alpha, but every command works and is verified on real hardware.
> Nothing has been tried against a real *system* disk yet, and the recovery ISO
> has been built but never booted. `restore` and `part move` write to disks and
> are not undoable — read what they print before saying yes.

## Why VHDX

The format choice carries most of the feature list, so it's worth being explicit:

| What the paid tools charge for | What bulkhead does |
|---|---|
| Block-level snapshot of a live volume | `Win32_ShadowCopy` → read the shadow device raw |
| Whole-disk image that boots | GPT copied verbatim; VHDX sized to match the source |
| Incremental chains | VHDX **differencing disks** — Windows tracks the blocks |
| Mount-image-as-a-drive | `AttachVirtualDisk` — it shows up in Explorer |
| Bootable recovery media | WinPE (ADK) + a `startnet.cmd` |
| Restore to bare metal | Copy the partition back; WinPE boots the VHDX directly |

A proprietary image format is how you get locked in. A VHDX opens in Explorer,
in Hyper-V, in `Mount-DiskImage`, and in every other tool on the platform — with
or without bulkhead installed.

## Install

Needs Rust and an **elevated** prompt (raw volume access).

```
cargo build --release
target\release\bulkhead.exe
```

## Usage

```
bulkhead image <VOL|diskN> <OUT.vhdx> [--from <PARENT.vhdx>] [--no-snapshot]
bulkhead mount <IMAGE.vhdx> [--rw]
bulkhead unmount <IMAGE.vhdx>
bulkhead restore <IMAGE.vhdx> <diskN> [--yes]
bulkhead part list <diskN>
bulkhead part move <diskN> <N> --to <OFFSET> [--yes]
bulkhead scan <diskN> [--rebuild] [--yes]
bulkhead undo <diskN> <TABLE.bin> [--yes]
bulkhead undelete <VOL|diskN> --to <DIR> [--at <OFFSET>]
bulkhead carve <VOL|diskN> --to <DIR> [--limit <N>]
bulkhead gui
bulkhead media <OUT.iso>
```

```powershell
# full image of the system volume, taken live via VSS
bulkhead image C: D:\backups\c-full.vhdx

# incremental: a differencing disk that only stores what changed
bulkhead image C: D:\backups\c-mon.vhdx --from D:\backups\c-full.vhdx

# browse it. read-only by default; it appears as a drive in Explorer
bulkhead mount D:\backups\c-mon.vhdx
bulkhead unmount D:\backups\c-mon.vhdx
```

`--no-snapshot` reads the volume directly instead of through VSS. Only for
volumes nothing is writing to — an offline disk, or a drive you just plugged in.

### Recovery media

```powershell
bulkhead media D:\bulkhead-recovery.iso
```

Builds bootable WinPE with bulkhead in it. Needs the **Windows ADK** and its
**WinPE add-on** — two separate downloads from <https://aka.ms/adk>, because
WinPE stopped shipping inside the ADK at 1809.

WinPE has no PowerShell in its base image, and bulkhead partitions its target
with the Storage cmdlets, so the media adds `WinPE-WMI`, `WinPE-NetFX`,
`WinPE-Scripting`, `WinPE-PowerShell` and `WinPE-StorageWMI`. That is most of
the build time and most of the ISO size.

VSS does not exist in WinPE either, so imaging from the media is always
`--no-snapshot`. That costs nothing: nothing in WinPE is writing to the disk
you are imaging.

Takes a few minutes, almost all of it DISM, and produces a ~534 MB ISO. For a
USB stick instead of an ISO, the workspace is left in place:

```powershell
MakeWinPEMedia /UFD "$env:TEMP\bulkhead-winpe" F:
```

## Status

`smoke.ps1` builds a throwaway 512 MB NTFS volume, images it, takes an
incremental, mounts the image back and compares a file hash across both. Run it
elevated.

Measured on that volume -- 496 MB with 106 MB in use, one small file added
between the two images:

| | before used-clusters-only | after |
|---|---|---|
| full image | 256 MB | **38 MB** |
| incremental | 255 MB | **35 MB** |
| reported as changed | 18.2 MB | **2.6 MB** |
| whole-disk image | - | **36 MB** (512 MB disk) |

`restore` puts that 512 MB image onto a 1 GB disk and relocates the GPT, so all
512 MB of the extra space comes back as usable free space rather than being
stranded behind a partition table describing the old disk.

- [x] `image <VOL>` — VSS snapshot → dynamic VHDX, GPT + one partition
- [x] `image diskN` — whole disk: verbatim GPT, per-partition VSS, raw gaps.
      Same size as the source, so it attaches and boots directly
- [x] `image --from` — differencing disk, unchanged blocks left unallocated
- [x] `mount` / `unmount` — `AttachVirtualDisk`, read-only by default
- [x] **used-clusters only** (`FSCTL_GET_VOLUME_BITMAP`) — free space is never
      read and never written
- [x] `restore` — erase a disk and write an image back, GPT relocated if the
      target is bigger. Refuses the disk hosting the running system
- [x] `scan` / `scan --rebuild` — find filesystems whose partition table is
      gone and write a new GPT pointing at them
- [x] `undelete` — recover deleted files from NTFS, resident and non-resident
- [x] `undo` — put back a table saved by `scan --rebuild`
- [x] `carve` — signature-based recovery when no filesystem survives
- [ ] verify (hash the image against the source)
- [ ] scheduling, retention, chain merge
- [x] WinPE recovery media (`bulkhead media`) — ISO; `/UFD` for USB
- [x] GUI (`bulkhead gui`) — native Win32, no toolkit, runs in WinPE
- [ ] MBR disks in `part` (GPT only today)
- [ ] tested against a real system disk; BitLocker images as ciphertext
- [ ] the ISO has been built but never booted

## Roadmap

The five things people currently pay for. Each one builds on the last:

1. **Imaging + recovery media** — **done.** The Reflect Free replacement.
   Outstanding: `verify`, scheduling/retention/chain merge.
2. **Partition manager** — **done for GPT.** `part move` is the operation
   nobody gives away; see below for what is deliberately left to Windows.
   Outstanding: MBR disks are rejected outright.
3. **Data recovery** — *in progress.* `scan` finds filesystems whose partition
   table is gone and rebuilds it, which is TestDisk's headline feature.
   **done.** `scan` rebuilds a lost partition table, `undelete` recovers files
   from a surviving MFT, `carve` works from raw signatures when there is none,
   and `bulkhead gui` fronts all of it.
4. **Filesystem drivers** — ext4/XFS/APFS/HFS+ read support. Needed for #3
   anyway; exposing it as a mountable volume (WinFsp) is nearly free once the
   parser exists.
5. **Certified secure erase** — ATA Secure Erase / NVMe Sanitize plus a signed
   PDF. Every lease return needs the paperwork. A weekend of work; sold for
   real money per drive.

Linux and macOS are a stretch goal. Windows first, because that's where the
gap is.

## Partitioning

```
> bulkhead part list disk6
disk 6: 1.0 GB (512-byte sectors)
  1       17.0 KB     16.0 MB  Microsoft reserved partition
  2       16.0 MB    495.9 MB  Basic data partition
         511.9 MB    512.0 MB  (free)

> bulkhead part move disk6 2 --to 116MB
[*] moving 495.9 MB (backwards)
  100%  495.9 MB / 495.9 MB
[+] partition 2 now starts at 116.0 MB

> bulkhead part list disk6
disk 6: 1.0 GB (512-byte sectors)
  1       17.0 KB     16.0 MB  Microsoft reserved partition
          16.0 MB    100.0 MB  (free)
  2      116.0 MB    495.9 MB  Basic data partition
         611.9 MB    412.0 MB  (free)
```

"backwards" there is not cosmetic: that move slides the partition forward by
less than its own length, so it overlaps itself and a front-to-back copy would
read bytes it had already overwritten.

Most of a partition manager is already free, so bulkhead only implements the
part that is not:

| Operation | Who does it |
|---|---|
| Shrink / extend a volume | Windows: `Resize-Partition`, Disk Management |
| MBR→GPT on the system disk | Windows: `mbr2gpt.exe`, since 1703 |
| **Move a partition** | **Nobody, at any price. This.** |

Moving is also the missing half of the operation people actually hit: Windows
will not extend a partition into free space that sits to its *left*. Slide the
partition down with `part move`, then extend it with the native tools.

A move is not journalled. If it is interrupted, the partition is gone — the
table is only rewritten after the data has landed, so a crash leaves the old
table pointing at data that is still where it says, but a crash *during* an
overlapping move loses the overlap. Image the disk first.

`part move` refuses the disk holding the running system; do that from the
recovery media.

## Recovering a lost partition table

```powershell
bulkhead scan disk2              # read-only: what is actually on there
bulkhead scan disk2 --rebuild    # write a table pointing at it
```

A partition table is a few kilobytes of pointers. Losing it does not touch the
filesystems — each one still opens with a header saying what it is and how big
it is, so scanning for those headers reconstructs the table. Thirteen
signatures, ported from [partrevive](https://github.com/sp00nznet/partrevive):
NTFS, exFAT, FAT12/16/32, ext2/3/4, btrfs, XFS, F2FS, swap, plus LUKS and LVM
which are reported but never sized (you cannot size a container from its
header).

Every detector re-reads the device to confirm the magic and returns the
volume's **own** recorded size, so nothing is truncated by a guess. Candidates
must start on a sector boundary, which kills almost every false positive from
file contents.

Two things a naive signature scan gets wrong, both handled:

- **Backup superblocks.** ext, XFS, FAT and f2fs keep spare copies of their
  superblock inside the filesystem, and a copy re-derives the same size — so it
  looks like an identical partition starting partway into the real one. Same
  type, same size, contained in an earlier candidate is always a copy.
- **Ghosts.** A disk that was repartitioned still carries the old layout's boot
  sectors, reporting plausible sizes for filesystems that have moved or gone.
  Size cannot separate them — a moved volume and the boot sector it left behind
  report exactly the same length. So NTFS candidates are corroborated twice:
  the boot sector is followed to the `$MFT` (required), and to the backup boot
  sector on the volume's last sector (raises confidence). A ghost's header
  survives; the tail it points at now belongs to whatever occupies that ground.

Where two candidates still claim the same ground, the best-corroborated wins,
then the larger, and the other is reported as skipped.

Rebuild writes the GPT itself, and only the table's own sectors. Neither
`Clear-Disk` nor `New-Partition` is used: the first is documented as erasing all
data on the disk, and the second zeroes the first sectors of a partition it
creates so stale filesystem metadata is not picked up. Both are right for
managing a disk and both destroy the thing a recovery tool exists to find. A truncated volume whose
tail is gone is still found — it just loses a tie-break rather than being
rejected outright. `--rebuild` saves the existing table to a file first,
and only writes partition entries — filesystem contents are never touched.

## Recovering deleted files

```powershell
bulkhead undelete D: --to C:
ecovered
bulkhead undelete disk2 --at 116MB --to C:
ecovered   # volume that will not mount
```

Deleting a file on NTFS clears one flag in its MFT record and marks its
clusters free. The record, the name, and the map of where the data lives all
survive until something reuses them — which is why this works at all, and why
it stops working the moment you keep using the volume.

Read-only on the source. Small files live inside their MFT record and come back
whole; larger ones are read back through their data runs. What comes off the
platter is whatever is there **now**: those clusters were released on delete, so
anything written since may be sitting in them. Check what you get.

NTFS only. Compressed and encrypted files are not decoded, and a file whose
record has been reused is gone for good. A file that cannot be fully read is
reported as PARTIAL with the amount that was readable, never padded out to its
recorded length — a correctly-sized file of zeros looks like a success and is
the worst thing a recovery tool can hand back.

## Carving: the last resort

```powershell
bulkhead carve disk2 --to C:\carved
```

When the MFT is gone there are no names, no sizes and no maps — only bytes.
Most formats announce themselves with a magic number and many mark their own
end, so a file can be lifted out whole without knowing anything about the
filesystem that held it. Fourteen signatures: JPEG, PNG, GIF, PDF, zip (which
covers Office and OpenDocument), MP4, SQLite, 7z, RAR, MP3, Ogg, gzip, bzip2.

Two things it cannot do. Carved files have **no names** — only the offset they
came from. And each is one contiguous stretch, so anything the filesystem
**fragmented** comes back truncated at the first gap. Use `undelete` whenever
the MFT survives; carve only when it does not.

## The window

```powershell
bulkhead gui
```

Native Win32 controls, no toolkit, no new dependencies — so it runs anywhere
USER32 does, including WinPE, where the recovery media actually needs it.

The window never touches a disk. Every button runs bulkhead as a child process
and pipes its output into the log, so the GUI can get the arguments wrong but
never the engine. **Destructive commands are deliberately absent** — `restore`,
`part move` and `scan --rebuild` stay on the command line, where their
confirmations are.

## Design notes

`ponytail:` comments in the source mark deliberate shortcuts and name their
ceiling. Two worth knowing about:

- **PowerShell** drives VSS and partition creation. It's the shortest path and
  exists on every live Windows, but it's absent from a minimal WinPE and costs
  ~400 ms a call. Goes away when we have direct `IVssBackupComponents` and our
  own GPT writer (the partition manager needs a GPT writer regardless).
- **Incrementals read-compare** rather than tracking changed blocks. Correct,
  and it keeps the child VHDX small, but it reads the parent in full. A real CBT
  filter driver is a lot of driver for something that's I/O-bound either way.

Two granularity numbers, because both were bugs once:

- **Comparison granularity is 64 KiB.** At the 4 MiB read-chunk size, one
  changed byte of NTFS metadata dirtied all 4 MiB, and a volume with nothing
  but background churn reported 492 MB of 496 MB changed.
- **VHDX block size is 2 MiB, not the 32 MiB default.** VHDX materialises a
  whole block on any write into it, and a differencing child inherits the size
  from its parent. The default turned 18 MB of scattered changes into 256 MB.

Neither is measured against a real workload yet; both are `ponytail:` marked.

### Known limitation: incremental size

An incremental is no smaller than `2 MiB x (number of distinct 2 MiB regions
touched)`, because VHDX materialises a whole block on any write into it. NTFS
churn -- `$LogFile`, `$MFT`, the volume bitmap -- is scattered rather than
contiguous, so a small number of changed bytes still touches a lot of regions.
Reading the allocation bitmap removes the free-space half of this; what remains
is metadata churn inside the used region.

A third granularity, alongside the two above: **free space is skipped in whole
4 MiB chunks**, so a run of free clusters shorter than that is still copied.

## License

MIT.
