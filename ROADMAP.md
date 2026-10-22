# Roadmap

What's next, what's parked, and what bulkhead deliberately won't do. What
works today is in the [README](README.md); this file is only intent.

bulkhead replaces five things people pay for: imaging with recovery media,
a partition manager, data recovery, reading Linux and Mac filesystems, and
secure erase with a certificate. Each has a working core. What's left is
proving the rest on real hardware and making it easier to use, not adding
breadth.

## Next

0. **Two data-loss bugs, found by a QA pass on 2026-10-22.** Imaging a
   volume through VSS reuses the MFT records of deleted files, so undelete
   can't find them afterwards, on the source or in the image: recover first,
   or image without a snapshot when the volume isn't the running system.
   And `undelete`/`carve` will write their output onto the volume they're
   recovering from; the engine should refuse that.
1. **`verify`.** Hash an image against its source, and test-mount it. "Will
   this restore?" is the whole question for a backup, and right now the answer
   is "probably".
2. **The GUI usability pass.** The window is how most people get the program,
   and it's the entry point on the worst day. From the same QA pass: it needs
   elevation up front (a manifest), confirmations that describe the disk, not
   just `disk2`, a disk list that shows bus, serial and "holds Windows",
   buttons for the hints it currently gives as CLI commands (`--rebuild`,
   `Set-Disk`), a clear done/failed/cancelled ending, and Tab navigation.
3. **A physical machine, end to end.** Image a real system disk through VSS,
   boot the recovery media on that machine, restore to a spare drive and boot
   it. Only VMs have done the whole cycle so far.
4. **Finish erase.** Print a SHA-256 of the certificate's JSON on the printed
   page, so the paper is tied to the record. Run NVMe sanitize and format on a
   real drive (NVMe Identify currently fails on the drives tested), and crypto
   scramble on a drive that offers it.
5. **MBR to GPT for non-system disks.** Windows' `mbr2gpt` only converts a
   system disk; nothing free converts a data or Linux disk.

## Later

- Extend a partition, including growing ext4 and XFS, which Windows won't touch.
- Merge a differencing chain back into one image.
- Stream large carved files instead of buffering them whole (the 2 GB cap).

## Parked until someone asks

New filesystem readers (F2FS, btrfs, APFS, UFS2), shrinking a partition,
reading BitLocker volumes as plaintext (today they image as ciphertext), and
Linux and macOS builds. Open an issue if you need one of these.

## Out of scope

- **Scheduling, retention, service accounts, fleets.** bulkhead is the desktop
  build: a person at one machine. For a scheduled backup, run `bulkhead image`
  from Task Scheduler. Unattended work belongs in a separate program built on
  the same library, not in this one.
- **Anything Windows already does well:** shrinking an NTFS partition,
  creating and formatting volumes. See [Partitioning](docs/partitioning.md).
- **A proprietary image format.** Images are VHDX, which open without bulkhead.
