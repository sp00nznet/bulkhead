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
bulkhead identify <VOL|diskN> [--at <OFFSET>]
bulkhead erase-info <diskN>
bulkhead erase <diskN> [--method <M>] [--yes]
bulkhead ls <VOL|diskN> [PATH] [--at <OFFSET>]
bulkhead cp <VOL|diskN> <PATH> --to <DIR> [--at <OFFSET>]
bulkhead mount-fs <VOL|diskN|IMAGE> <X:> [--at <OFFSET>]
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
- [x] `ls` / `cp` — read ext2/3/4, XFS and HFS+ volumes Windows cannot mount
- [x] `identify` — RAID/LVM/ZFS/btrfs/bcachefs membership and format recognition
- [x] `mount-fs` — ext4/XFS/HFS+ as a read-only Windows drive, via WinFsp
- [x] `erase-info` — what erase commands a drive supports, and what blocks them
- [x] `erase --method overwrite` — zero every sector, then sample-verify
- [ ] `erase` via firmware sanitize — written, never run against a drive
- [ ] erase certificate (JSON + signed PDF)
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
4. **Filesystem drivers** — *in progress.* `ls` and `cp` read ext2/3/4, XFS
   and HFS+; `identify` recognises MD RAID, LVM2, ZFS, btrfs, bcachefs,
   SquashFS, UFS2 and VMFS members. Outstanding: F2FS and UFS2 reading, APFS,
   btrfs reading, and exposing them all as mountable volumes via WinFsp.
5. **Certified secure erase** — *in progress.* `erase-info` reports what a
   drive supports and what blocks it; `erase --method overwrite` wipes a drive
   and verifies by sampling, confirmed on real hardware. The ATA SANITIZE path
   (crypto scramble, block erase) is written but has never run against a drive.
   Outstanding: proving sanitize on hardware, the NVMe equivalents, and the
   certificate.

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

## Reading ext2/3/4, XFS and HFS+

```powershell
bulkhead ls disk2 --at 1MB              # what is on the Linux partition
bulkhead ls disk2 --at 1MB /home/nedch
bulkhead cp disk2 /home/nedch --to C:\out --at 1MB
```

These are the filesystems Paragon charges per seat to read. The filesystem is
detected from the volume, so `ls` and `cp` take the same form either way.
`--at` is the partition's byte offset, which `bulkhead part list` or
`bulkhead scan` will tell you; a mounted volume letter needs no offset.

ext4 is the straightforward one: a superblock gives the layout, group
descriptors locate the inode tables, each inode carries a tree of extents. XFS
is big-endian, packs an allocation-group index into the top of every block and
inode number, and stores extents as bitfields straddling two 64-bit words —
so most of its work is shifting fields apart before they mean anything. HFS+
puts every directory entry on the volume in one B-tree keyed by parent folder
and name, with each node's record offsets stored backwards at the end of it.

**Read-only, deliberately and permanently.** Writing ext4 safely means
implementing its journal, and a half-understood journal is how filesystems get
destroyed. bulkhead reads these; Linux writes them.

Not yet, and refused with a clear message rather than misread: ext2/ext3
volumes predating extents (indirect block maps), XFS files large enough to need
b-tree forks, HFS+ forks continuing into the extents overflow file, and old HFS
volumes with HFS+ embedded inside them. APFS and btrfs are next.

### Mounting them as a drive

```powershell
bulkhead mount-fs disk2 X: --at 1MB
```

Makes an ext4, XFS or HFS+ volume a read-only Windows drive, so Explorer and
every other program can open it. Ctrl-C unmounts.

A read-only filesystem still has to supply `Create` and `Overwrite`: WinFsp
checks that all of Create, Open and Overwrite exist before dispatching *any*
create, including opening an existing file for reading. Both answer
`STATUS_MEDIA_WRITE_PROTECTED`.

This needs **WinFsp** (`winget install WinFsp.WinFsp`), the only dependency
here that does not ship with Windows. It is loaded at runtime rather than
linked, so every other command works without it and building bulkhead needs no
SDK. WinFsp is GPLv3 with an exception for free software, which bulkhead's MIT
licence falls under; a proprietary fork would need a licence from its authors.

### Verified against the real thing

`fs-smoke.ps1` builds images with `mkfs` inside WSL, fills them with known
content, records the hashes, then reads them back with bulkhead and compares.
Unit tests only prove the parsing agrees with itself; this proves it agrees
with the filesystem's own implementation, which is the only opinion that
counts.

| | |
|---|---|
| ext4 | **pass** — text, a 300 KB binary spanning extents, a nested file |
| ext4 via a mounted drive | **pass** — the same three, read back through `X:` |
| XFS | **pass** — same three |
| XFS via a mounted drive | **pass** |
| ext2 | **refused correctly** — no extents, so it declines rather than guessing |
| F2FS, HFS+ | skipped: the WSL kernel cannot mount them, so no image can be filled |

It found a real bug. XFS listed directories and reported correct file sizes but
returned every file **empty** — the `NREXT64` feature, on by default in current
`mkfs.xfs`, moves `di_nextents`, and reading the old offset gives zero. Extents
are now bounded by the fork itself rather than by a count field that moves
between versions. No unit test would have caught it: it was written against the
same wrong assumption as the code.

## What is this disk?

```powershell
bulkhead identify disk3
```

The question you actually have when someone hands you an unlabelled drive out
of a dead NAS. Reading a filesystem is a lot of work; recognising one — and
recognising the RAID or volume-manager layer *underneath* it — is very little,
and on a NAS disk that layer is the thing standing between you and any
filesystem at all.

| Recognised | Reports |
|---|---|
| **Linux MD RAID** | array name and UUID, level, which member this disk is, chunk size, data offset, event count |
| **LVM2 PV** | PV UUID, volume group name, device size |
| **ZFS** | pool name and GUID, this device's GUID, state, txg, last host |
| **btrfs** | label, filesystem UUID, device id, how many devices the set needs |
| **bcachefs** | label, UUID, device N of M |
| **SquashFS** | version, inode count, compression |
| **UFS2** | block size, last mount point, volume name |
| **VMFS** | version and label |
| **NTFS / exFAT / FAT** | named only — Windows reads these itself |

It probes the whole device and then every partition on it, because a NAS disk
carries its RAID metadata on the partition rather than the disk.

**Event counts and txg numbers are the point.** Members of the same array with
different ones are out of sync, and assembling them in the wrong order is how a
recoverable array becomes an unrecoverable one.

**Two things can claim the same disk, and only one of them is current.** A
drive that was a ZFS member and has since been reformatted still carries its
vdev labels at the far end, where nothing has written. So `identify` reports
*which* of ZFS's four labels survive: front labels gone and end labels intact
means the pool membership is history, not news. It reports everything it finds
and tells you what the evidence is — it does not pick a winner for you.

These are identification only. ZFS, VMFS and SquashFS are not read by bulkhead
— SquashFS contents are always compressed, and the other two are large projects
in themselves. Assemble MD/LVM on Linux, import ZFS with `zpool`, and the
filesystem on top is then readable here.

## Secure erase

```powershell
bulkhead erase-info disk3
```

Blancco and KillDisk charge per drive for one command the drive already
implements, plus a piece of paper. The command is the easy part. The parts
worth building are knowing *which* command a given drive will accept, and
producing a record afterwards that means something — so that comes first, and
it is read-only.

It reports the drive's identity and every erase path it offers: ATA security
erase, ATA sanitize (crypto/block/overwrite), NVMe format, NVMe sanitize. More
usefully it says what is **stopping** one:

- **FROZEN** — nearly every desktop firmware freezes the ATA security state at
  boot, and a security erase cannot start until the drive is power-cycled.
  Suspend and resume, or hot-plug it. ATA *sanitize* is a separate feature set
  and is not affected, which is why it is preferred where available.
- **USB** — bridges rarely pass these commands through, and one that
  half-implements them can report success without erasing anything.
- **password already set** — an existing ATA password must be known first.
- **the drive did not answer** — some storage drivers, Intel RST and VMD
  especially, do not pass capability queries through at all. That is reported
  as *unknown*, never as "this drive cannot be erased": a question that was
  never asked is not a negative answer.

```powershell
bulkhead erase disk5 --method overwrite
```

Writes zeros over every sector, then reads back 32 points spread across the
drive — including the first and last, where partition tables live — and refuses
to claim success unless they all come back blank. It samples the same points
*before* the write too, so it can say whether anything was actually removed or
the drive was already empty.

It asks for the drive's **serial number**, not a yes. A serial cannot be typed
by reflex, and finding it means looking at which drive this really is.

Where the drive offers a real sanitize, that is used instead and the drive
erases its own media:

```powershell
bulkhead erase disk1 --method ata-sanitize-crypto   # discards the key, seconds
bulkhead erase disk1 --method ata-sanitize-block    # erases every block
```

**Overwrite is the weaker method and is labelled as such wherever it appears.**
A firmware sanitize reaches blocks the drive has quietly remapped out of
service over its life; an overwrite reaches only what the drive currently maps.
On flash — SSDs, SD cards, USB sticks — wear levelling can leave old data in
spare blocks that no write will ever land on. If a drive advertises a sanitize
and you ask for an overwrite anyway, it says so before it starts.

Verification matches the method rather than assuming one shape. An overwrite or
a block erase has to read back blank. A crypto scramble does not and never
will: it throws the key away, so the media still reads as dense ciphertext.
Checking that for blankness would fail a successful erase, so what gets
verified is that the old contents are gone — and the output says plainly that
the key's destruction is the drive's claim, not a thing bulkhead observed.

The sanitize path is written against the ACS spec but has not yet run against a
drive. The overwrite path has: a 7.4 GB USB card reader was imaged, erased,
checked with `identify` and `scan` (both found nothing -- no table, no
filesystem signatures anywhere on the device), then restored from the image
with its contents intact.

That test is worth describing precisely, because it proves less than it looks
like it does. It shows the mapped sectors were blank afterwards and held data
before. It does not show the card retains no data at all -- an overwrite cannot
reach blocks the controller has remapped, and nothing observable from the host
can tell you whether any exist. For flash, only a firmware sanitize makes the
stronger claim.

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
