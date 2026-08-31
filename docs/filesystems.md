# Reading ext2/3/4, XFS and HFS+

```powershell
bulkhead ls disk2 --at 1MB              # what is on the Linux partition
bulkhead ls disk2 --at 1MB /home/user
bulkhead cp disk2 /home/user --to C:\out --at 1MB
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

## Mounting them as a drive

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

## Verified against the real thing

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

---

[< back to the README](../README.md)
