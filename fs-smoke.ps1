# Filesystem read tests against REAL volumes, not synthetic structures.
#
# Builds images with mkfs inside WSL, fills them with known content, records
# the hashes, then reads them back with bulkhead and compares. The unit tests
# check that the parsing logic is self-consistent; this checks it agrees with
# the filesystem's own implementation, which is the only opinion that counts.
#
# Needs WSL with a distro that has the mkfs tools. Does not need elevation:
# everything happens in image files.
# Continue, not Stop: cargo, wsl and bulkhead all write progress to stderr,
# and Stop treats any of it as a fatal error. Every external call below checks
# its exit code explicitly instead.
$ErrorActionPreference = 'Continue'

$exe  = Join-Path $PSScriptRoot 'target\debug\bulkhead.exe'
$work = Join-Path $PSScriptRoot 'target\fs-tests'
$distro = 'Debian'

Push-Location $PSScriptRoot
try {
    cargo build 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally { Pop-Location }

New-Item -ItemType Directory -Force $work | Out-Null
# WSL sees the workspace through /mnt/<drive>.
$wslWork = '/mnt/' + $PSScriptRoot.Substring(0,1).ToLower() + `
           ($PSScriptRoot.Substring(2) -replace '\\','/') + '/target/fs-tests'

function Invoke-Wsl($script) {
    # PowerShell here-strings carry CRLF; sh reads the CR as part of the
    # argument and rejects "set -e" as an illegal option.
    $script = $script -replace "`r`n", "`n"
    $out = wsl -d $distro -u root -e sh -c $script 2>&1
    if ($LASTEXITCODE -ne 0) { throw "wsl failed:`n$($out -join "`n")" }
    $out
}

# Does this distro have the tool to build this filesystem?
function Have($tool) {
    wsl -d $distro -u root -e sh -c "command -v $tool >/dev/null 2>&1"
    $LASTEXITCODE -eq 0
}

# Making an image is not enough -- the WSL kernel has to be able to mount it to
# put known content inside. A filesystem it cannot mount cannot be verified
# here, and saying so is better than pretending it passed.
function CanMount($fsname) {
    wsl -d $distro -u root -e sh -c "grep -qw $fsname /proc/filesystems || (modprobe $fsname 2>/dev/null && grep -qw $fsname /proc/filesystems)"
    $LASTEXITCODE -eq 0
}

$results = @()

function Test-Filesystem($name, $mkfs, $mkfsArgs, $sizeMb, $kmod) {
    if (-not (Have $mkfs)) {
        Write-Host "[-] $name skipped: $mkfs not installed in $distro" -ForegroundColor DarkGray
        $script:results += [pscustomobject]@{ Name = $name; Result = 'skipped (no mkfs)' }
        return
    }
    if (-not (CanMount $kmod)) {
        Write-Host "[-] $name skipped: the $distro kernel cannot mount $kmod, so no image can be filled" -ForegroundColor DarkGray
        $script:results += [pscustomobject]@{ Name = $name; Result = 'skipped (kernel)' }
        return
    }
    Write-Host "`n=== $name ===" -ForegroundColor Cyan
    $img = "$wslWork/$name.img"

    # Known content: a text file, a binary big enough to need several extents,
    # and something two directories down.
    $hashes = Invoke-Wsl @"
set -e
rm -f $img
dd if=/dev/zero of=$img bs=1M count=$sizeMb status=none
$mkfs $mkfsArgs $img >/dev/null 2>&1
mkdir -p /tmp/bhmnt
mount -o loop $img /tmp/bhmnt
mkdir -p /tmp/bhmnt/docs/nested
seq 1 500 | sed 's/^/line /' > /tmp/bhmnt/hello.txt
head -c 300000 /dev/urandom > /tmp/bhmnt/docs/blob.bin
echo 'deep file' > /tmp/bhmnt/docs/nested/deep.txt
sync
md5sum /tmp/bhmnt/hello.txt /tmp/bhmnt/docs/blob.bin /tmp/bhmnt/docs/nested/deep.txt
umount /tmp/bhmnt
"@
    $want = @{}
    foreach ($l in $hashes) {
        if ($l -match '^([0-9a-f]{32})\s+/tmp/bhmnt/(.+)$') { $want[$Matches[2]] = $Matches[1] }
    }
    if ($want.Count -ne 3) { throw "expected 3 hashes from mkfs, got $($want.Count)" }

    $local = Join-Path $work "$name.img"
    $out = Join-Path $work "$name-out"
    Remove-Item $out -Recurse -Force -ErrorAction SilentlyContinue

    $lsOut = & $exe ls $local 2>&1 | Out-String
    Write-Host $lsOut
    if ($LASTEXITCODE -ne 0) { throw "${name}: ls failed" }

    # docs/ holds blob.bin and nested/deep.txt. A listing that shows it as a
    # bare name is how a volume with a VM image on it got read as empty, so
    # the depth has to show or this is not a listing worth erasing on.
    if ($lsOut -notmatch 'docs/\s+2 files') {
        throw "${name}: ls did not report docs/ as holding 2 files -- nested content is invisible again"
    }

    & $exe cp $local / --to $out 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "${name}: cp failed" }

    # Compare every file bulkhead produced against what the filesystem's own
    # tools said it contained.
    $bad = 0
    foreach ($k in $want.Keys) {
        $got = Get-ChildItem $out -Recurse -File | Where-Object { $_.FullName -like "*$($k -replace '/','\')" }
        if (-not $got) { Write-Host "  MISSING $k" -ForegroundColor Red; $bad++; continue }
        $h = (Get-FileHash $got.FullName -Algorithm MD5).Hash.ToLower()
        if ($h -ne $want[$k]) {
            Write-Host "  MISMATCH $k`n    want $($want[$k])`n    got  $h" -ForegroundColor Red
            $bad++
        } else {
            Write-Host "  ok $k" -ForegroundColor Green
        }
    }
    $script:results += [pscustomobject]@{ Name = $name; Result = if ($bad) { "$bad wrong" } else { 'pass' } }
    if ($bad) { throw "${name}: ${bad} file(s) did not match" }

    # And again through a real drive letter, if WinFsp is installed. Reading
    # the same bytes back through Explorer's own path is the only proof the
    # filesystem driver side works.
    if (-not (Test-Path 'C:\Program Files (x86)\WinFsp\bin\winfsp-x64.dll')) {
        Write-Host "  (mount check skipped: WinFsp not installed)" -ForegroundColor DarkGray
        return
    }
    # Never a hardcoded letter: mounting over a mapped network drive or an
    # existing volume would shadow it, and the test would then be reading
    # someone else's filesystem.
    $used = (Get-PSDrive -PSProvider FileSystem).Name
    $letter = (73..90 | ForEach-Object { [char]$_ } |
               Where-Object { $_ -notin $used } | Select-Object -First 1)
    if (-not $letter) { throw "no free drive letter to mount on" }
    $letter = "${letter}:"
    Write-Host "  mounting on $letter (first free letter)" -ForegroundColor DarkGray
    $proc = Start-Process $exe -ArgumentList @('mount-fs', $local, $letter) `
                          -PassThru -WindowStyle Hidden
    try {
        for ($i = 0; $i -lt 15 -and -not (Test-Path "$letter\hello.txt"); $i++) {
            Start-Sleep -Seconds 1
        }
        if (-not (Test-Path "$letter\hello.txt")) { throw "${name}: mount produced no drive" }
        $mbad = 0
        foreach ($k in $want.Keys) {
            # Join-Path rather than string concatenation: the separator has
            # been mangled by escaping twice already.
            $path = Join-Path "$letter\" ($k -replace '/', [char]92)
            if (-not (Test-Path $path)) { Write-Host "  MOUNT MISSING $k" -ForegroundColor Red; $mbad++; continue }
            $h = (Get-FileHash $path -Algorithm MD5).Hash.ToLower()
            if ($h -ne $want[$k]) {
                Write-Host "  MOUNT MISMATCH $k`n    want $($want[$k])`n    got  $h" -ForegroundColor Red
                $mbad++
            } else {
                Write-Host "  ok (mounted) $k" -ForegroundColor Green
            }
        }
        if ($mbad) { throw "${name}: ${mbad} file(s) wrong through the mount" }
        $script:results += [pscustomobject]@{ Name = "$name via drive"; Result = 'pass' }
    } finally {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
    }
}

Test-Filesystem 'ext4' 'mkfs.ext4'     '-q -L BULKTEST -F' 64  'ext4'
Test-Filesystem 'xfs'  'mkfs.xfs'      '-q -L BULKTEST -f' 300 'xfs'
Test-Filesystem 'f2fs' 'mkfs.f2fs'     '-q -l BULKTEST'    100 'f2fs'
Test-Filesystem 'hfs'  'mkfs.hfsplus'  '-v BULKTEST'       64  'hfsplus'

# ext2 has no extents, and must be refused rather than misread -- a plausible
# wrong answer is worse than a clear refusal.
Write-Host "`n=== ext2 (expected to refuse) ===" -ForegroundColor Cyan
if (Have 'mkfs.ext2') {
    Invoke-Wsl @"
set -e
rm -f $wslWork/ext2.img
dd if=/dev/zero of=$wslWork/ext2.img bs=1M count=32 status=none
mkfs.ext2 -q -L OLDSKOOL -F $wslWork/ext2.img
"@ | Out-Null
    $msg = & $exe ls (Join-Path $work 'ext2.img') 2>&1 | Out-String
    if ($msg -match 'indirect block') {
        Write-Host "  ok refused with a clear reason" -ForegroundColor Green
        $results += [pscustomobject]@{ Name = 'ext2'; Result = 'refused (correct)' }
    } else {
        throw "ext2 should have been refused, got:`n$msg"
    }
}

Write-Host "`n--- summary ---"
$results | Format-Table -AutoSize | Out-String | Write-Host
if ($results | Where-Object {
        $_.Result -ne 'pass' -and $_.Result -notlike 'skipped*' -and $_.Result -ne 'refused (correct)'
    }) {
    throw "FAIL"
}
Write-Host "ALL FILESYSTEM TESTS PASSED" -ForegroundColor Green

exit 0
