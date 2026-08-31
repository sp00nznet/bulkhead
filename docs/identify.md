# What is this disk?

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

---

[< back to the README](../README.md)
