#!/bin/sh
# Build the conformance corpus: one image per filesystem, made and filled by
# that filesystem's own tools, plus a manifest of what went in. bulkhead's
# readers are then checked against the manifest (tools/conformance.ps1), which
# is the only opinion that counts: the unit tests prove the parsers agree with
# themselves, this proves they agree with mkfs and the kernel.
#
# Linux, as root (it loop-mounts). CI runs it on ubuntu-latest; fs-smoke.ps1
# runs it in WSL. Usage: sudo tools/corpus.sh <outdir>
#
# A filesystem this machine can't make or mount gets a SKIP manifest, never a
# fake pass. WSL's kernel can't mount hfsplus, for one.
set -u
out=$1
mkdir -p "$out"
mnt=$(mktemp -d)

build() { # name mkfs "args" size_mb kernel_fs
    name=$1 mkfs=$2 args=$3 size=$4 fs=$5
    img=$out/$name.img man=$out/$name.manifest
    rm -f "$img"
    if ! command -v "$mkfs" >/dev/null 2>&1; then
        echo "SKIP $mkfs not installed" >"$man"; return
    fi
    dd if=/dev/zero of="$img" bs=1M count="$size" status=none
    if ! $mkfs $args "$img" >/dev/null 2>&1; then
        echo "SKIP $mkfs failed" >"$man"; rm -f "$img"; return
    fi
    if ! mount -o loop -t "$fs" "$img" "$mnt" 2>/dev/null; then
        echo "SKIP this kernel cannot mount $fs" >"$man"; rm -f "$img"; return
    fi
    # A text file, a binary big enough to need several extents, something two
    # directories down, and 64 directories to push inodes past block group 0
    # (an ext4 descriptor-size bug once broke every inode outside it).
    mkdir -p "$mnt/docs/nested"
    seq 1 500 | sed 's/^/line /' >"$mnt/hello.txt"
    head -c 300000 /dev/urandom >"$mnt/docs/blob.bin"
    echo 'deep file' >"$mnt/docs/nested/deep.txt"
    for i in $(seq 1 64); do mkdir -p "$mnt/spread/d$i"; echo "$i" >"$mnt/spread/d$i/f.txt"; done
    sync
    {
        echo "FILECOUNT $(find "$mnt" -type f | wc -l)"
        (cd "$mnt" && md5sum hello.txt docs/blob.bin docs/nested/deep.txt)
    } >"$man"
    umount "$mnt"
}

build ext4 mkfs.ext4    '-q -L BULKTEST -F' 320 ext4
build xfs  mkfs.xfs     '-q -L BULKTEST -f' 320 xfs
build hfs  mkfs.hfsplus '-v BULKTEST'       64  hfsplus

# ext2 has no extents. bulkhead must refuse it with a reason, not misread it:
# a plausible wrong answer is worse than a clear refusal.
if command -v mkfs.ext2 >/dev/null 2>&1; then
    rm -f "$out/ext2.img"
    dd if=/dev/zero of="$out/ext2.img" bs=1M count=32 status=none
    mkfs.ext2 -q -L OLDSKOOL -F "$out/ext2.img" >/dev/null 2>&1
    echo "REFUSE indirect block" >"$out/ext2.manifest"
else
    echo "SKIP mkfs.ext2 not installed" >"$out/ext2.manifest"
fi

rmdir "$mnt"
ls -l "$out"
