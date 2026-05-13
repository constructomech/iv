# Package iv.exe and its required DLLs into a portable directory.
#
# Usage:
#   .\scripts\package-portable.ps1                    # default: dist\iv-portable\
#   .\scripts\package-portable.ps1 -Output some\dir   # custom output dir
#   .\scripts\package-portable.ps1 -SkipBuild         # don't run cargo build first
#
# The output directory is wiped and recreated. After packaging you can
# zip the folder, copy it to another machine that has the Microsoft
# Visual C++ Redistributable installed, and run iv.exe directly.

[CmdletBinding()]
param(
    [string]$Output = "dist\iv-portable",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

# Locate the repo root by climbing from this script's directory.
$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $repoRoot

if (-not $SkipBuild) {
    Write-Host "Building iv.exe (release)..."
    & cargo build --release --bin iv
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed (exit code $LASTEXITCODE). If iv.exe is running, close it and retry."
    }
}

$exe = Join-Path $repoRoot "target\release\iv.exe"
$dllRoot = Join-Path $repoRoot "target\vcpkg\installed\x64-windows\bin"

if (-not (Test-Path $exe)) {
    throw "iv.exe not found at $exe. Run without -SkipBuild to build it."
}
if (-not (Test-Path $dllRoot)) {
    throw "Native DLLs not found at $dllRoot. Have you run 'cargo vcpkg build' and the libheif/ffmpeg install command from README.md?"
}

# Minimum DLL set actually loaded at runtime. Skipping unused DLLs that vcpkg
# also installs (avdevice, avfilter, swresample, pkgconf) keeps the portable
# under ~30 MB.
$requiredDlls = @(
    # HEIC stack
    "heif.dll",
    "libde265.dll",
    "aom.dll",
    # Video + HEVC decoder plugin
    "avcodec-62.dll",
    "avformat-62.dll",
    "avutil-60.dll",
    "swscale-9.dll"
)

# Resolve the output directory to an absolute path and wipe it.
$outDir = if ([System.IO.Path]::IsPathRooted($Output)) {
    $Output
} else {
    Join-Path $repoRoot $Output
}
if (Test-Path $outDir) {
    Remove-Item -LiteralPath $outDir -Recurse -Force
}
New-Item -ItemType Directory -Path $outDir -Force | Out-Null

Write-Host "Packaging into $outDir ..."

Copy-Item -LiteralPath $exe -Destination (Join-Path $outDir "iv.exe")

$missing = @()
foreach ($dll in $requiredDlls) {
    $src = Join-Path $dllRoot $dll
    if (Test-Path $src) {
        Copy-Item -LiteralPath $src -Destination (Join-Path $outDir $dll)
    } else {
        $missing += $dll
    }
}

if ($missing.Count -gt 0) {
    throw ("Missing required DLLs in {0}:`n  {1}" -f $dllRoot, ($missing -join "`n  "))
}

# Include licensing info so the portable can be redistributed.
$licenses = Join-Path $repoRoot "LICENSES.md"
$license = Join-Path $repoRoot "LICENSE"
if (Test-Path $licenses) { Copy-Item -LiteralPath $licenses -Destination $outDir }
if (Test-Path $license)  { Copy-Item -LiteralPath $license  -Destination $outDir }

# Report the result.
$items = Get-ChildItem -LiteralPath $outDir
$totalBytes = ($items | Measure-Object -Property Length -Sum).Sum
$totalMb = [math]::Round($totalBytes / 1MB, 1)
Write-Host ""
Write-Host ("Done. {0} files, {1} MB total." -f $items.Count, $totalMb)
Write-Host ""
$items | Sort-Object Name | ForEach-Object {
    Write-Host ("  {0,-22} {1,10:N0} bytes" -f $_.Name, $_.Length)
}
Write-Host ""
Write-Host "Run with:"
Write-Host ("  & '{0}' <folder-with-photos>" -f (Join-Path $outDir "iv.exe"))
