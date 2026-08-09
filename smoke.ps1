# Smoke test: build a throwaway 512 MB NTFS volume, image it, mount the image
# back, and compare a file hash across both. Run elevated.
#
# Detaches everything on the way out, including after a failure -- a leaked
# attached VHDX makes the next run fail on cleanup instead of on the bug.
#
# ponytail: diskpart, not New-VHD -- New-VHD needs the Hyper-V module, diskpart
# ships with every Windows.
$ErrorActionPreference = 'Stop'

$work = Join-Path $env:TEMP 'bulkhead-smoke'
$src  = Join-Path $work 'src.vhdx'
$img  = Join-Path $work 'image.vhdx'
$inc  = Join-Path $work 'image-inc.vhdx'
$dsk  = Join-Path $work 'disk.vhdx'
$tgt  = Join-Path $work 'restore-target.vhdx'
$exe  = Join-Path $PSScriptRoot 'target\debug\bulkhead.exe'

# Build here rather than trusting whatever is in target\ -- `cargo test` leaves
# target\debug\bulkhead.exe stale, which silently tests the previous build.
Push-Location $PSScriptRoot
try { cargo build; if ($LASTEXITCODE -ne 0) { throw "cargo build failed" } }
finally { Pop-Location }
if (-not (Test-Path $exe)) { throw "missing $exe" }

function Invoke-Diskpart($lines, [switch]$Quiet) {
    $f = Join-Path $env:TEMP 'bulkhead-dp.txt'
    Set-Content $f (($lines + 'exit') -join "`r`n") -Encoding ascii
    $out = diskpart /s $f
    if ($LASTEXITCODE -ne 0 -and -not $Quiet) { throw "diskpart failed:`n$($out -join "`n")" }
}

function Detach-All {
    foreach ($v in @($src, $img, $inc, $dsk, $tgt)) {
        if (Test-Path $v) {
            Invoke-Diskpart @("select vdisk file=`"$v`"", "detach vdisk") -Quiet
        }
    }
}

Detach-All
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $work | Out-Null

try {
    Write-Host "[*] creating source volume $src"
    Invoke-Diskpart @(
        "create vdisk file=`"$src`" maximum=512 type=expandable",
        "attach vdisk",
        "convert gpt",
        "create partition primary",
        "format fs=ntfs quick label=BULKSRC",
        "assign"
    )

    $srcLetter = (Get-Volume -FileSystemLabel BULKSRC).DriveLetter
    Write-Host "[*] source volume is ${srcLetter}:"

    # something to look for on the other side
    1..50 | ForEach-Object { "payload line $_" } | Set-Content "${srcLetter}:\hello.txt"
    $srcHash = (Get-FileHash "${srcLetter}:\hello.txt").Hash

    # 64 MB of allocated clusters on a 496 MB volume, so "free space skipped"
    # has something to be measured against
    fsutil file createnew "${srcLetter}:\bulk.dat" (64MB) | Out-Null

    Write-Host "`n[*] bulkhead image"
    & $exe image "${srcLetter}:" $img
    if ($LASTEXITCODE -ne 0) { throw "image failed" }

    # change one small file, so "changed" has a known expected magnitude:
    # a few MB of NTFS metadata churn, not the whole volume
    "second payload" | Set-Content "${srcLetter}:\second.txt"

    Write-Host "`n[*] bulkhead image --from (incremental, one small file added)"
    & $exe image "${srcLetter}:" $inc --from $img
    if ($LASTEXITCODE -ne 0) { throw "incremental failed" }

    Write-Host "`n[*] bulkhead mount"
    & $exe mount $img
    if ($LASTEXITCODE -ne 0) { throw "mount failed" }
    Start-Sleep -Seconds 2

    $imgLetter = (Get-Volume -FileSystemLabel BULKSRC |
                  Where-Object DriveLetter -ne $srcLetter).DriveLetter
    if (-not $imgLetter) { throw "image attached but no volume appeared" }
    Write-Host "[*] image volume is ${imgLetter}:"

    $imgHash = (Get-FileHash "${imgLetter}:\hello.txt").Hash
    Write-Host "`n[*] source   $srcHash"
    Write-Host "[*] image    $imgHash"

    if ($srcHash -ne $imgHash) { throw "FAIL  hashes differ" }
    Write-Host "`nPASS  volume image round-trips" -ForegroundColor Green
    Write-Host ("      full {0:N1} MB / incremental {1:N1} MB" -f `
        ((Get-Item $img).Length / 1MB), ((Get-Item $inc).Length / 1MB))

    & $exe unmount $img
    Start-Sleep -Seconds 1

    # --- whole-disk image -------------------------------------------------
    # The source VHDX is a real GPT disk: ESP-less, but with a data partition
    # Windows has mounted and a second one it has not, so the raw path, the
    # snapshot path and the inter-partition gaps all get exercised.
    $srcDisk = (Get-Volume -FileSystemLabel BULKSRC | Get-Partition | Get-Disk).Number
    Write-Host "`n[*] bulkhead image disk$srcDisk (whole disk)"
    & $exe image "disk$srcDisk" $dsk
    if ($LASTEXITCODE -ne 0) { throw "disk image failed" }

    & $exe mount $dsk
    if ($LASTEXITCODE -ne 0) { throw "disk image mount failed" }
    Start-Sleep -Seconds 2

    $dskLetter = (Get-Volume -FileSystemLabel BULKSRC |
                  Where-Object DriveLetter -ne $srcLetter).DriveLetter
    if (-not $dskLetter) { throw "disk image attached but no volume appeared" }
    $dskHash = (Get-FileHash "${dskLetter}:\hello.txt").Hash
    Write-Host "[*] disk image volume is ${dskLetter}:  $dskHash"
    & $exe unmount $dsk

    if ($dskHash -ne $srcHash) { throw "FAIL  disk image hashes differ" }
    Write-Host "`nPASS  whole-disk image round-trips" -ForegroundColor Green
    Write-Host ("      disk image {0:N1} MB (source disk 512 MB)" -f `
        ((Get-Item $dsk).Length / 1MB))

    # --- restore ----------------------------------------------------------
    # Onto a 1 GB target, so the GPT has to be relocated: a verbatim copy would
    # leave the backup table stranded at the 512 MB mark and the extra space
    # unaddressable. Detach the source first -- restoring a disk verbatim
    # duplicates its GUIDs, and Windows will hold one of the pair offline.
    Invoke-Diskpart @("select vdisk file=`"$src`"", "detach vdisk") -Quiet

    $before = @(Get-Disk).Number
    Invoke-Diskpart @("create vdisk file=`"$tgt`" maximum=1024 type=expandable", "attach vdisk")
    $tgtDisk = @(Get-Disk).Number | Where-Object { $_ -notin $before }
    if (-not $tgtDisk) { throw "target vdisk attached but no new disk appeared" }
    Write-Host "`n[*] bulkhead restore -> disk$tgtDisk (1 GB target, 512 MB image)"

    & $exe restore $dsk "disk$tgtDisk" --yes
    if ($LASTEXITCODE -ne 0) { throw "restore failed" }
    Start-Sleep -Seconds 3

    $rp = Get-Partition -DiskNumber $tgtDisk | Where-Object DriveLetter | Select-Object -First 1
    if (-not $rp) {
        # nothing auto-mounted it; give the data partition a letter ourselves
        $rp = Get-Partition -DiskNumber $tgtDisk |
              Where-Object { $_.Size -gt 100MB } | Select-Object -First 1
        if (-not $rp) { throw "restored disk has no data partition" }
        $rp | Add-PartitionAccessPath -AssignDriveLetter
        Start-Sleep -Seconds 2
        $rp = Get-Partition -DiskNumber $tgtDisk -PartitionNumber $rp.PartitionNumber
    }
    $rHash = (Get-FileHash "$($rp.DriveLetter):\hello.txt").Hash
    Write-Host "[*] restored volume is $($rp.DriveLetter):  $rHash"
    if ($rHash -ne $srcHash) { throw "FAIL  restored hashes differ" }

    # The relocated GPT is what makes the extra 500 MB addressable at all.
    $free = (Get-Disk -Number $tgtDisk).LargestFreeExtent
    Write-Host ("[*] free space on the restored disk: {0:N1} MB" -f ($free / 1MB))
    if ($free -lt 400MB) {
        throw "FAIL  GPT was not extended; only $([int]($free/1MB)) MB usable of the extra 512 MB"
    }

    Write-Host "`nPASS  restore to a larger disk, GPT relocated" -ForegroundColor Green

    # --- partition move ----------------------------------------------------
    # The restored disk has a 16 MB partition 1 then the data partition, with
    # ~500 MB free at the end. Slide the data partition right, into space that
    # is not adjacent to it -- overlapping its own start, which is the case a
    # naive front-to-back copy corrupts.
    Write-Host "`n[*] bulkhead part list disk$tgtDisk"
    & $exe part list "disk$tgtDisk"

    $dataPart = Get-Partition -DiskNumber $tgtDisk | Sort-Object Size -Descending | Select-Object -First 1
    $oldOffset = $dataPart.Offset
    $newOffset = $oldOffset + 100MB
    Write-Host "`n[*] bulkhead part move disk$tgtDisk $($dataPart.PartitionNumber) --to $newOffset"

    & $exe part move "disk$tgtDisk" $dataPart.PartitionNumber --to $newOffset --yes
    if ($LASTEXITCODE -ne 0) { throw "part move failed" }
    Start-Sleep -Seconds 3

    $moved = Get-Partition -DiskNumber $tgtDisk |
             Where-Object { $_.PartitionNumber -eq $dataPart.PartitionNumber }
    if ($moved.Offset -ne $newOffset) {
        throw "FAIL  partition is at $($moved.Offset), expected $newOffset"
    }
    if ($moved.Size -ne $dataPart.Size) {
        throw "FAIL  move changed the size: $($dataPart.Size) -> $($moved.Size)"
    }

    # The filesystem has to survive being relocated wholesale, not just the
    # table entry. If the data did not follow, this volume will not mount.
    if (-not $moved.DriveLetter) {
        $moved | Add-PartitionAccessPath -AssignDriveLetter
        Start-Sleep -Seconds 2
        $moved = Get-Partition -DiskNumber $tgtDisk -PartitionNumber $dataPart.PartitionNumber
    }
    $mHash = (Get-FileHash "$($moved.DriveLetter):\hello.txt").Hash
    Write-Host "[*] moved volume is $($moved.DriveLetter):  $mHash"
    if ($mHash -ne $srcHash) { throw "FAIL  data did not survive the move" }

    Write-Host "`n[*] bulkhead part list disk$tgtDisk (after)"
    & $exe part list "disk$tgtDisk"

    Write-Host "`nPASS  partition moved, filesystem intact" -ForegroundColor Green

    # --- scan and rebuild --------------------------------------------------
    # Destroy the partition table but not the data, which is exactly what a
    # bad `clean`, a botched installer or a corrupt GPT leaves behind.
    #
    # This disk is a deliberately nasty case: the move above copied the volume
    # from 16 MB to 116 MB without erasing the source, so the old NTFS boot
    # sector is still sitting at 16 MB claiming the same size. The scan has to
    # tell the live filesystem from that ghost.
    Write-Host "`n[*] wiping the partition table on disk$tgtDisk (data left alone)"
    Invoke-Diskpart @("select disk $tgtDisk", "clean")
    Start-Sleep -Seconds 2

    if (Get-Partition -DiskNumber $tgtDisk -ErrorAction SilentlyContinue) {
        throw "table should be gone but partitions are still listed"
    }

    Write-Host "`n[*] bulkhead scan disk$tgtDisk --rebuild"
    & $exe scan "disk$tgtDisk" --rebuild --yes
    if ($LASTEXITCODE -ne 0) { throw "scan --rebuild failed" }
    Start-Sleep -Seconds 3

    $rec = Get-Partition -DiskNumber $tgtDisk | Sort-Object Size -Descending | Select-Object -First 1
    if (-not $rec) { throw "FAIL  rebuild produced no partitions" }
    if ($rec.Offset -ne $newOffset) {
        throw "FAIL  rebuilt partition at $($rec.Offset), expected the live one at $newOffset"
    }
    if (-not $rec.DriveLetter) {
        $rec | Add-PartitionAccessPath -AssignDriveLetter
        Start-Sleep -Seconds 2
        $rec = Get-Partition -DiskNumber $tgtDisk -PartitionNumber $rec.PartitionNumber
    }

    # A freshly created partition entry can take a few seconds to surface as a
    # mounted volume, so poll rather than assume. If it never arrives, dump
    # what Windows actually thinks is there instead of failing blind.
    $recPath = "$($rec.DriveLetter):\hello.txt"
    for ($i = 0; $i -lt 10 -and -not (Test-Path $recPath); $i++) { Start-Sleep -Seconds 1 }
    if (-not (Test-Path $recPath)) {
        Write-Host "`n--- partitions on disk$tgtDisk ---"
        Get-Partition -DiskNumber $tgtDisk |
            Format-Table PartitionNumber, DriveLetter, Offset, Size, Type -AutoSize | Out-String | Write-Host
        Write-Host "--- volumes ---"
        Get-Volume | Where-Object DriveLetter |
            Format-Table DriveLetter, FileSystemLabel, FileSystem, Size, SizeRemaining -AutoSize |
            Out-String | Write-Host
        Write-Host "--- root of $($rec.DriveLetter): ---"
        Get-ChildItem "$($rec.DriveLetter):\" -Force -ErrorAction SilentlyContinue |
            Format-Table Name, Length -AutoSize | Out-String | Write-Host

        # Is the filesystem still on the platter, or did we destroy it?
        # A RAW volume means Windows would not mount it; it does not say why.
        Write-Host "--- re-scan (is the NTFS still there?) ---"
        & $exe scan "disk$tgtDisk"

        # If it is there, the remaining suspect is the volume manager holding a
        # stale view. Detaching and reattaching forces a clean re-read.
        Write-Host "--- detach/reattach, then look again ---"
        Invoke-Diskpart @("select vdisk file=`"$tgt`"", "detach vdisk") -Quiet
        Invoke-Diskpart @("select vdisk file=`"$tgt`"", "attach vdisk")
        Start-Sleep -Seconds 4
        Get-Disk | Where-Object { $_.Location -like "*$tgt*" } | Get-Partition |
            Format-Table PartitionNumber, DriveLetter, Offset, Size -AutoSize |
            Out-String | Write-Host
        Get-Volume | Where-Object DriveLetter |
            Format-Table DriveLetter, FileSystemLabel, FileSystem, Size -AutoSize |
            Out-String | Write-Host

        throw "FAIL  $recPath never appeared"
    }
    $recHash = (Get-FileHash $recPath).Hash
    Write-Host "[*] recovered volume is $($rec.DriveLetter):  $recHash"
    if ($recHash -ne $srcHash) { throw "FAIL  recovered data does not match" }

    Write-Host "`nPASS  lost partition table rebuilt from filesystem headers" -ForegroundColor Green

    # --- undelete ----------------------------------------------------------
    # Delete a file with known contents, then get it back. hello.txt is small
    # enough to be resident inside its MFT record; bulk.dat is not, so both the
    # resident and the data-run paths get exercised.
    $vol = "$($rec.DriveLetter):"
    $gone = "$vol\deleteme.txt"
    1..200 | ForEach-Object { "recover me $_" } | Set-Content $gone
    $goneHash = (Get-FileHash $gone).Hash
    $goneSize = (Get-Item $gone).Length
    Remove-Item $gone -Force
    Remove-Item "$vol\bulk.dat" -Force -ErrorAction SilentlyContinue
    # Flush, or the MFT change may still be sitting in cache
    Write-VolumeCache -DriveLetter $rec.DriveLetter -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2

    $recovered = Join-Path $work 'recovered'
    Write-Host "`n[*] bulkhead undelete $vol --to $recovered"
    & $exe undelete $vol --to $recovered
    if ($LASTEXITCODE -ne 0) { throw "undelete failed" }

    $hit = Get-ChildItem $recovered -ErrorAction SilentlyContinue |
           Where-Object { $_.Name -like "*deleteme.txt" -and $_.Length -eq $goneSize } |
           Select-Object -First 1
    if (-not $hit) {
        Write-Host "--- recovered files ---"
        Get-ChildItem $recovered -ErrorAction SilentlyContinue |
            Format-Table Name, Length -AutoSize | Out-String | Write-Host
        throw "FAIL  deleteme.txt ($goneSize bytes) was not recovered"
    }
    $hitHash = (Get-FileHash $hit.FullName).Hash
    Write-Host "[*] deleted  $goneHash"
    Write-Host "[*] recovered $hitHash  ($($hit.Name))"
    if ($hitHash -ne $goneHash) { throw "FAIL  recovered contents differ" }

    Write-Host "`nPASS  deleted file recovered byte-for-byte" -ForegroundColor Green
}
finally {
    Detach-All
}
