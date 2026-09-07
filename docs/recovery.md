# Data recovery

Three tools, in the order you reach for them: rebuild the table if it is
gone, recover files from a surviving MFT, carve from raw bytes when there is
none.

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
bulkhead undelete D: --to C:\recovered
bulkhead undelete disk2 --at 116MB --to C:\recovered   # volume that will not mount
```

Deleting a file on NTFS clears one flag in its MFT record and marks its
clusters free. The record, the name, and the map of where the data lives all
survive until something reuses them — which is why this works at all, and why
it stops working the moment you keep using the volume.

Read-only on the source. Files are written into their original directory
tree, rebuilt from each record's parent reference -- deleted directories
included, since a removed tree frees its directory records along with its
files. A chain that cannot be followed to the root (parent record reused, or
a stale reference that now cycles) puts the file under `_orphans/` with
whatever partial path did resolve, rather than dropping it or guessing.
`--flat` writes everything into one directory instead.

Small files live inside their MFT record and come back
whole; larger ones are read back through their data runs. What comes off the
platter is whatever is there **now**: those clusters were released on delete, so
anything written since may be sitting in them. Check what you get.

NTFS only. Compressed and encrypted files are not decoded, and a file whose
record has been reused is gone for good. A file that cannot be fully read is
reported as PARTIAL with the amount that was readable, never padded out to its
recorded length — a correctly-sized file of zeros looks like a success and is
the worst thing a recovery tool can hand back.

## What this has been run against

Not a synthetic test: a stray `rm -rf` with a computed path removed roughly 48
project trees from an 18 TB NTFS volume, and this is what got them back.

| | |
|---|---|
| recovered | **815,472 files, 432.6 GB** |
| git repositories | 238 |
| corrupt objects across all of them | **2** |
| zero-length (record survived, data did not) | 4,176 -- 0.5% |
| MFT records scanned | 1,003,008 |

The interesting part is not the size, it is that the result could be **checked
against values known from somewhere else**. Most recovery tools can only tell
you they wrote something; git can tell you whether what came back is bit-for-bit
the thing that was lost, because every object is named by the hash of its own
contents. Four independent checks, by four different mechanisms:

- A packfile recovered **without its `.idx`** was re-indexed from scratch: 2,248
  objects, 1,580 deltas resolved, and the computed hash came out equal to its own
  filename. A pack is named by the SHA-1 of its contents, so that is proof of a
  bit-exact recovery rather than a plausibility check. Six packs were verified
  this way; none failed.
- `git fsck` on the recovered repository: **clean**, no output. Every object's
  hash matches its content and connectivity is complete.
- A branch that existed on no remote anywhere came back with **992 commits
  reachable and 117 ahead of master** -- the same 117 measured before the
  deletion.
- Two ordinary files matched their known blob hashes: one against a public
  GitHub copy, and one *locally modified* file against the hash recorded in a
  `git diff` taken before the loss.

Two objects out of 238 repositories were corrupt -- their clusters had been
reused before recovery. Both were repairable from a second copy: one from
another recovered repo that held the same commit, one from GitHub. Worth knowing
generally: **a corrupt object is a per-object problem, not a per-repo one**, and
any other clone can rewrite it.

What this run does *not* establish: `undelete` has no resume, so an interrupted
scan restarts from record 0; and how many files come back PARTIAL depends
entirely on how much was written to the volume after the deletion. The single
most important thing is still to stop writing to the volume -- all of the above
was possible only because nothing had reused those clusters.

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

---

[< back to the README](../README.md)
