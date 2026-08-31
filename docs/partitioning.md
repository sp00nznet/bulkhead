# Partitioning

```
> bulkhead part list disk6
disk 6: 1.0 GB (512-byte sectors, GPT)
  1       17.0 KB     16.0 MB  Microsoft reserved partition
  2       16.0 MB    495.9 MB  Basic data partition
         511.9 MB    512.0 MB  (free)

> bulkhead part move disk6 2 --to 116MB
[*] moving 495.9 MB (backwards)
  100%  495.9 MB / 495.9 MB
[+] partition 2 now starts at 116.0 MB

> bulkhead part list disk6
disk 6: 1.0 GB (512-byte sectors, GPT)
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
| MBR→GPT on the *system* disk | Windows: `mbr2gpt.exe`, since 1703 |
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

---

[< back to the README](../README.md)
