# Design notes

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

## Known limitation: incremental size

An incremental is no smaller than `2 MiB x (number of distinct 2 MiB regions
touched)`, because VHDX materialises a whole block on any write into it. NTFS
churn -- `$LogFile`, `$MFT`, the volume bitmap -- is scattered rather than
contiguous, so a small number of changed bytes still touches a lot of regions.
Reading the allocation bitmap removes the free-space half of this; what remains
is metadata churn inside the used region.

A third granularity, alongside the two above: **free space is skipped in whole
4 MiB chunks**, so a run of free clusters shorter than that is still copied.

---

[< back to the README](../README.md)
