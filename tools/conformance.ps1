# Conformance harness (REPO_RULES section 9): bulkhead against ground truth.
#
#   - every image in the corpus (tools/corpus.sh) is listed, copied out and
#     hashed against what mkfs and the kernel put in it;
#   - smoke.ps1's stages (image, incremental, restore, part move, scan
#     --rebuild, undelete, carve) on throwaway VHDX disks. Needs elevation.
#
# Prints one PASS/FAIL/SKIP line per check and a count, then compares against
# tools/conformance-baseline.txt: a check in the baseline that no longer
# passes fails the run, even if everything else is fine. When a new check
# passes, add its name to the baseline so it can't quietly regress later.
#
#   tools\conformance.ps1 -Corpus <dir> [-Exe <bulkhead.exe>] [-NoSmoke]
param(
    [Parameter(Mandatory)][string]$Corpus,
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\bulkhead.exe'),
    [switch]$NoSmoke
)
# Continue, not Stop: bulkhead writes progress to stderr, and Stop treats any
# of it as fatal. Every call below checks its exit code instead.
$ErrorActionPreference = 'Continue'
$Exe = (Resolve-Path $Exe).Path
$results = [ordered]@{}

function Record($name, $status, $why = '') {
    $script:results[$name] = $status
    $line = if ($why) { "$status $name -- $why" } else { "$status $name" }
    $color = @{ PASS = 'Green'; FAIL = 'Red'; SKIP = 'DarkGray' }[$status]
    Write-Host $line -ForegroundColor $color
}

function Test-Image($name, $img, $manifest) {
    $lines = @(Get-Content $manifest)  # @(): a one-line file is a string, not an array
    if ($lines[0] -match '^SKIP (.*)') { Record "$name" SKIP $Matches[1]; return }

    $ls = & $Exe ls $img 2>&1 | Out-String
    if ($lines[0] -match '^REFUSE (.*)') {
        if ($ls -match $Matches[1]) { Record "$name refused" PASS }
        else { Record "$name refused" FAIL "expected a refusal naming '$($Matches[1])', got: $ls" }
        return
    }

    $wantFiles = [int]($lines[0] -replace '^FILECOUNT\s+', '')
    $want = @{}
    foreach ($l in $lines | Select-Object -Skip 1) {
        if ($l -match '^([0-9a-f]{32})\s+\*?(.+)$') { $want[$Matches[2]] = $Matches[1] }
    }
    # Nothing to compare against would make every content check pass vacuously.
    if ($want.Count -ne 3) { Record "$name" FAIL "manifest has $($want.Count) hashes, expected 3"; return }

    # A total that matches the filesystem's own count is what catches inodes
    # read as garbage outside the first block group / allocation group.
    if ($LASTEXITCODE -ne 0) { Record "$name ls" FAIL "exit $LASTEXITCODE" }
    elseif ($ls -notmatch '(\d+) files in all') { Record "$name ls" FAIL 'no total file count' }
    elseif ([int]$Matches[1] -ne $wantFiles) { Record "$name ls" FAIL "counted $($Matches[1]), the filesystem has $wantFiles" }
    elseif ($ls -notmatch 'docs/\s+2 files') { Record "$name ls" FAIL 'docs/ not shown holding 2 files' }
    else { Record "$name ls" PASS }

    $out = Join-Path $env:TEMP "bulkhead-conf-$name"
    Remove-Item $out, "$out-carve" -Recurse -Force -ErrorAction SilentlyContinue
    & $Exe cp $img / --to $out 2>&1 | Out-Null
    $bad = @()
    foreach ($k in $want.Keys) {
        $got = Get-ChildItem $out -Recurse -File -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -like "*$($k -replace '/','\')" } | Select-Object -First 1
        if (-not $got) { $bad += "$k missing" }
        elseif ((Get-FileHash $got.FullName -Algorithm MD5).Hash.ToLower() -ne $want[$k]) { $bad += "$k differs" }
    }
    if ($bad) { Record "$name cp" FAIL ($bad -join ', ') } else { Record "$name cp" PASS }

    # These images hold no carvable signatures, so finding nothing is right;
    # opening an image file at all is the check (carve once refused them).
    & $Exe carve $img --to "$out-carve" --limit 2 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { Record "$name carve opens image" FAIL "exit $LASTEXITCODE" }
    else { Record "$name carve opens image" PASS }

    # The same bytes through a real drive letter, if WinFsp is installed.
    if (-not (Test-Path 'C:\Program Files (x86)\WinFsp\bin\winfsp-x64.dll')) {
        Record "$name mount-fs" SKIP 'WinFsp not installed'; return
    }
    # Never a hardcoded letter: mounting over an existing drive would shadow
    # it, and the check would then read someone else's filesystem.
    $used = (Get-PSDrive -PSProvider FileSystem).Name
    $letter = (73..90 | ForEach-Object { [char]$_ } | Where-Object { $_ -notin $used } | Select-Object -First 1)
    if (-not $letter) { Record "$name mount-fs" FAIL 'no free drive letter'; return }
    $proc = Start-Process $Exe -ArgumentList @('mount-fs', $img, "${letter}:") -PassThru -WindowStyle Hidden
    try {
        for ($i = 0; $i -lt 15 -and -not (Test-Path "${letter}:\hello.txt"); $i++) { Start-Sleep 1 }
        $bad = @()
        foreach ($k in $want.Keys) {
            $p = Join-Path "${letter}:\" ($k -replace '/', '\')
            if (-not (Test-Path $p)) { $bad += "$k missing" }
            elseif ((Get-FileHash $p -Algorithm MD5).Hash.ToLower() -ne $want[$k]) { $bad += "$k differs" }
        }
        if ($bad) { Record "$name mount-fs" FAIL ($bad -join ', ') } else { Record "$name mount-fs" PASS }
    } finally {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep 2
    }
}

foreach ($m in Get-ChildItem $Corpus -Filter *.manifest | Sort-Object Name) {
    $name = $m.BaseName
    Test-Image $name (Join-Path $Corpus "$name.img") $m.FullName
}

if (-not $NoSmoke) {
    # smoke.ps1 stops at the first failure, so every stage after it reports
    # FAIL too: that is the truth, they never ran.
    $stages = @(
        'volume image round-trips'
        'whole-disk image round-trips'
        'restore to a larger disk, GPT relocated'
        'partition moved, filesystem intact'
        'lost partition table rebuilt from filesystem headers'
        'deleted file recovered byte-for-byte'
        'file carved from raw bytes by signature'
    )
    $log = Join-Path $env:TEMP 'bulkhead-smoke.log'
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot '..\smoke.ps1') *>&1 |
        Tee-Object -FilePath $log | Out-Null
    $smoke = Get-Content $log -Raw
    foreach ($s in $stages) {
        if ($smoke -match [regex]::Escape("PASS  $s")) { Record "smoke: $s" PASS }
        else { Record "smoke: $s" FAIL "see $log" }
    }
}

$pass = @($results.Values | Where-Object { $_ -eq 'PASS' }).Count
$fail = @($results.Values | Where-Object { $_ -eq 'FAIL' }).Count
$skip = @($results.Values | Where-Object { $_ -eq 'SKIP' }).Count
Write-Host "`nconformance: $pass/$($results.Count) pass, $fail fail, $skip skipped"

$baseline = Get-Content (Join-Path $PSScriptRoot 'conformance-baseline.txt') |
    Where-Object { $_ -and -not $_.StartsWith('#') }
$lost = @($baseline | Where-Object { $results[$_] -ne 'PASS' })
$new = @($results.Keys | Where-Object { $results[$_] -eq 'PASS' -and $_ -notin $baseline })
if ($new) { Write-Host "newly passing, add to tools/conformance-baseline.txt:`n  $($new -join "`n  ")" }
if ($lost) { Write-Host "REGRESSION, these passed before:`n  $($lost -join "`n  ")" -ForegroundColor Red }
if ($lost -or $fail) { exit 1 }
exit 0
