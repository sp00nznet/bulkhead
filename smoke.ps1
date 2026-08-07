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
    foreach ($v in @($src, $img, $inc)) {
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
    Write-Host "`nPASS  image round-trips" -ForegroundColor Green
    Write-Host ("      full {0:N1} MB / incremental {1:N1} MB" -f `
        ((Get-Item $img).Length / 1MB), ((Get-Item $inc).Length / 1MB))
}
finally {
    Detach-All
}
