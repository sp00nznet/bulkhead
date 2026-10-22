# Filesystem read tests against REAL volumes, on this machine: the same checks
# CI runs (tools/conformance.ps1), with the corpus built in WSL instead of on
# a Linux runner.
#
# Needs WSL with a distro that has the mkfs tools. Does not need elevation:
# everything happens in image files. WSL's kernel can't mount hfsplus, so HFS+
# reports SKIP here; CI's Ubuntu kernel can.
$ErrorActionPreference = 'Continue'
$distro = 'Debian'
$work = Join-Path $PSScriptRoot 'target\fs-tests'

Push-Location $PSScriptRoot
try {
    cargo build 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally { Pop-Location }

New-Item -ItemType Directory -Force $work | Out-Null
# WSL sees the workspace through /mnt/<drive>.
function ToWsl($p) { '/mnt/' + $p.Substring(0, 1).ToLower() + ($p.Substring(2) -replace '\\', '/') }
wsl -d $distro -u root -e sh (ToWsl (Join-Path $PSScriptRoot 'tools\corpus.sh')) (ToWsl $work)
if ($LASTEXITCODE -ne 0) { throw "corpus.sh failed in WSL" }

& (Join-Path $PSScriptRoot 'tools\conformance.ps1') -Corpus $work `
    -Exe (Join-Path $PSScriptRoot 'target\debug\bulkhead.exe') -NoSmoke
exit $LASTEXITCODE
