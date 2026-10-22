# Roadmap

What's next, what's parked, and what bulkhead deliberately won't do. What
works today is in the [README](README.md); this file is only intent.

bulkhead replaces five things people pay for: imaging with recovery media,
a partition manager, data recovery, reading Linux and Mac filesystems, and
secure erase with a certificate. Each has a working core. What's left is
proving the rest on real hardware and making it easier to use, not adding
breadth.

## Next

1. **A physical machine, end to end.** Image a real system disk through VSS,
   boot the recovery media on that machine, restore to a spare drive and boot
   it. Only VMs have done the whole cycle so far.
2. **Finish erase.** Print a SHA-256 of the certificate's JSON on the printed
   page, so the paper is tied to the record. Run NVMe sanitize and format on a
   real drive (NVMe Identify currently fails on the drives tested), and crypto
   scramble on a drive that offers it.
3. **MBR to GPT for non-system disks.** Windows' `mbr2gpt` only converts a
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
