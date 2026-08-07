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

> ⚠️ Pre-alpha. `image`, `mount`, `unmount` work and round-trip on real
> hardware; everything else is unwritten. Nothing here writes to a source disk,
> but read the warnings before pointing it at anything you care about.

## Why VHDX

The format choice carries most of the feature list, so it's worth being explicit:

| What the paid tools charge for | What bulkhead does |
|---|---|
| Block-level snapshot of a live volume | `Win32_ShadowCopy` → read the shadow device raw |
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
bulkhead image <VOL> <OUT.vhdx> [--from <PARENT.vhdx>] [--no-snapshot]
bulkhead mount <IMAGE.vhdx> [--rw]
bulkhead unmount <IMAGE.vhdx>
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
bulkhead media D:ulkhead-recovery.iso
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

- [x] `image` — VSS snapshot → dynamic VHDX, GPT + one partition
- [x] `image --from` — differencing disk, unchanged blocks left unallocated
- [x] `mount` / `unmount` — `AttachVirtualDisk`, read-only by default
- [x] **used-clusters only** (`FSCTL_GET_VOLUME_BITMAP`) — free space is never
      read and never written
- [ ] `restore` — write a partition back to a live disk
- [ ] verify (hash the image against the source)
- [ ] scheduling, retention, chain merge
- [x] WinPE recovery media (`bulkhead media`) — ISO; `/UFD` for USB
- [ ] GUI

## Roadmap

The five things people currently pay for. Each one builds on the last:

1. **Imaging + recovery media** — *in progress.* The Reflect Free replacement.
2. **Partition manager** — resize/move, MBR↔GPT without data loss, migrate an
   OS to a smaller SSD. Inherits the GPT work from
   [partrevive](https://github.com/sp00nznet/partrevive).
3. **Data recovery GUI** — TestDisk and PhotoRec are free and capable; the UX
   is the product. Mostly integration, not new science.
4. **Filesystem drivers** — ext4/XFS/APFS/HFS+ read support. Needed for #3
   anyway; exposing it as a mountable volume (WinFsp) is nearly free once the
   parser exists.
5. **Certified secure erase** — ATA Secure Erase / NVMe Sanitize plus a signed
   PDF. Every lease return needs the paperwork. A weekend of work; sold for
   real money per drive.

Linux and macOS are a stretch goal. Windows first, because that's where the
gap is.

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
