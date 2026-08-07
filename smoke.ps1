# Smoke test: build a throwaway 512 MB NTFS volume, image it, mount the image
# back, and diff the files. Run elevated. Leaves nothing attached.
#
# ponytail: diskpart, not New-VHD -- New-VHD needs the Hyper-V module, diskpart
# ships with every Windows.
$ErrorActionPreference = 'Stop'

$work = Join-Path $env:TEMP 'bulkhead-smoke'
$src  = Join-Path $work 'src.vhdx'
$img  = Join-Path $work 'image.vhdx'
$inc  = Join-Path $work 'image-inc.vhdx'
$exe  = Join-Path $PSScriptRoot 'target\debug\bulkhead.exe'

if (-not (Test-Path $exe)) { throw "build first: cargo build   (missing $exe)" }
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory $work | Out-Null

function diskpart-run($lines) {
    $f = Join-Path $work 'dp.txt'
    Set-Content $f ($lines -join "`r`n") -Encoding ascii
    $out = diskpart /s $f
    if ($LASTEXITCODE -ne 0) { throw "diskpart failed:`n$($out -join "`n")" }
}

Write-Host "[*] creating source volume $src"
diskpart-run @(
    "create vdisk file=`"$src`" maximum=512 type=expandable",
    "attach vdisk",
    "convert gpt",
    "create partition primary",
    "format fs=ntfs quick label=BULKSRC",
    "assign",
    "exit"
)

$srcLetter = (Get-Volume -FileSystemLabel BULKSRC).DriveLetter
Write-Host "[*] source volume is ${srcLetter}:"

# something to look for on the other side
1..50 | ForEach-Object { "payload line $_" } | Set-Content "${srcLetter}:\hello.txt"
$srcHash = (Get-FileHash "${srcLetter}:\hello.txt").Hash

Write-Host "`n[*] bulkhead image"
& $exe image "${srcLetter}:" $img
if ($LASTEXITCODE -ne 0) { throw "image failed" }

Write-Host "`n[*] bulkhead image --from (incremental, nothing changed yet)"
& $exe image "${srcLetter}:" $inc --from $img
if ($LASTEXITCODE -ne 0) { throw "incremental failed" }

Write-Host "`n[*] bulkhead mount"
& $exe mount $img
if ($LASTEXITCODE -ne 0) { throw "mount failed" }
Start-Sleep -Seconds 2

$imgLetter = (Get-Volume -FileSystemLabel BULKSRC | Where-Object DriveLetter -ne $srcLetter).DriveLetter
if (-not $imgLetter) { throw "image mounted but no volume appeared" }
Write-Host "[*] image volume is ${imgLetter}:"

$imgHash = (Get-FileHash "${imgLetter}:\hello.txt").Hash
Write-Host "`n[*] source   $srcHash"
Write-Host "[*] image    $imgHash"

& $exe unmount $img
diskpart-run @("select vdisk file=`"$src`"", "detach vdisk", "exit")

if ($srcHash -eq $imgHash) {
    Write-Host "`nPASS  image round-trips" -ForegroundColor Green
    Write-Host ("      full {0:N1} MB / incremental {1:N1} MB" -f `
        ((Get-Item $img).Length / 1MB), ((Get-Item $inc).Length / 1MB))
} else {
    throw "FAIL  hashes differ"
}
