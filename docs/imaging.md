# Imaging, restore and recovery media

```powershell
# full image of the system volume, taken live through VSS
bulkhead image C: D:\backups\c-full.vhdx

# incremental: a differencing disk that only stores what changed
bulkhead image C: D:\backups\c-mon.vhdx --from D:\backups\c-full.vhdx

# a whole disk: every partition, the GPT copied verbatim
bulkhead image disk0 D:\backups\disk0.vhdx

# will it restore? See "Checking an image" below
bulkhead verify D:\backups\c-mon.vhdx

# browse an image. Read-only by default; it appears as a drive in Explorer
bulkhead mount D:\backups\c-mon.vhdx
bulkhead unmount D:\backups\c-mon.vhdx

# put a whole-disk image back. ERASES disk2, and asks first
bulkhead restore D:\backups\disk0.vhdx disk2
```

Only used clusters are read (`FSCTL_GET_VOLUME_BITMAP`): free space is never
read and never written.

## How a live volume is held still

If nothing has the volume open, `image` **locks** it and reads it directly:
nothing else can write to it, and nothing is written to it either. Only a
volume in use (the running C:, a folder open in Explorer) goes through a **VSS
snapshot**, and `image` says so, because a snapshot isn't free. VSS keeps its
change log as files on the volume being snapshotted, and NTFS reuses deleted
files' MFT records for them, so a snapshot can destroy files `undelete` would
have recovered. **If you need to undelete from a volume, do that before
imaging it**, or close whatever has it open so it can be locked. Versions
before 0.2.0 always snapshotted; a QA pass found the damage.

`--no-snapshot` skips both and reads the volume as it is. Use it only for
volumes nothing is writing to, such as an offline disk.

`image`, `undelete` and `carve` refuse to write their output onto the volume
or disk they're reading.

## Checking an image

`verify` attaches the image read-only, which opens its whole incremental
chain, so a parent that's missing or has changed fails here instead of on the
day you restore. It then reads every byte and opens every partition's
filesystem: NTFS by reading its whole MFT, ext4, XFS and HFS+ by walking every
directory.

VERIFY_SAMPLE

The SHA-256 is of the image. Run `verify` again later and the same digest
means nothing has changed. It doesn't compare the image with the source: a
live source has moved on since it was imaged, and those differences aren't
faults.

`restore` refuses the disk Windows is running from. Onto a bigger disk, it
moves the backup GPT to the new end, so the extra space comes back as free
space instead of being stranded behind a table that describes the old disk.

## What the sizes look like

Measured by `smoke.ps1` on a 496 MB NTFS volume with 106 MB in use, one small
file added between the two images:

| | before used-clusters-only | after |
|---|---|---|
| full image | 256 MB | **38 MB** |
| incremental | 255 MB | **35 MB** |
| reported as changed | 18.2 MB | **2.6 MB** |
| whole-disk image | - | **36 MB** (512 MB disk) |

Why an incremental isn't smaller than that is in [design notes](design.md).

## What has been run

- `smoke.ps1`'s stages run in CI on throwaway VHDX disks: volume and
  whole-disk image, verify (including a broken chain), restore onto a bigger
  disk, part move, scan --rebuild, undelete, undelete after imaging the same
  volume, and carve.
- A VM's Debian system disk was imaged from the recovery media, restored onto a
  bigger disk, and booted to a login prompt. `sfdisk -d` matched the source.
- A 238 GB laptop drive with 8 partitions was imaged, wiped, and restored.
- **Not yet:** the system disk of a physical machine, end to end. BitLocker
  volumes image as ciphertext.

## Recovery media

```powershell
bulkhead media D:\bulkhead-recovery.iso
```

Builds bootable WinPE with bulkhead and its window in it. Needs an elevated
prompt, the **Windows ADK** and its **WinPE add-on**: two separate downloads
from <https://aka.ms/adk>, because WinPE stopped shipping inside the ADK at
1809.

WinPE has no PowerShell in its base image, and bulkhead partitions its target
with the Storage cmdlets, so the media adds `WinPE-WMI`, `WinPE-NetFX`,
`WinPE-Scripting`, `WinPE-PowerShell` and `WinPE-StorageWMI`. That is most of
the build time and most of the ISO size. It also copies in:

- `comctl32.dll`, which WinPE lacks and the window needs;
- this machine's storage and network drivers, so the media can see the disks
  on the machine it was built on. Build it on the machine you'll recover.

VSS doesn't exist in WinPE, so imaging from the media is always
`--no-snapshot`. That costs nothing: nothing in WinPE is writing to the disk
you are imaging.

It takes a few minutes, almost all of it DISM. For a USB stick instead of an
ISO, the workspace is left in place:

```powershell
MakeWinPEMedia /UFD "$env:TEMP\bulkhead-winpe" F:
```

The media has been booted in a VM, with `identify`, `image`, `mount`, `restore`
and the window all run from it. It hasn't been booted on a physical machine
yet.

---

[< back to the README](../README.md)
