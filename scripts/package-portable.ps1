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

# Minimum DLL set actually loaded at runtime, including transitive imports.
# iv.exe LoadLibrary's heif.dll, avcodec/avformat/avutil/swscale. heif.dll's
# own DLL imports pull in libde265 (HEVC decoder), aom (AV1 decoder), and
# avcodec/avutil (FFmpeg HEVC decoder plugin). avcodec then transitively
# imports swresample. None of avdevice, avfilter, or pkgconf is referenced
# by anything we load.
$requiredDlls = @(
    # HEIC stack
    "heif.dll",
    "libde265.dll",
    "aom.dll",
    # Video + HEVC decoder plugin
    "avcodec-62.dll",
    "avformat-62.dll",
    "avutil-60.dll",
    "swresample-6.dll",
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

# Sanity check: scan every shipped binary for ASCII DLL references and make
# sure each non-system reference is either in the bundle or otherwise
# guaranteed to be available (system DLLs, GPU driver DLLs). This catches
# missed transitive imports automatically; the bundle previously shipped
# without swresample-6.dll because avcodec's transitive dep on it wasn't
# obvious from the build's top-level DLL list. The check is a coarse string
# scan, not a real PE-imports parse, so it can produce false positives
# (substrings of longer strings) — those go in $expectedSystem.
$expectedSystem = @(
    # Windows system DLLs that ship with every install we care about
    'kernel32', 'user32', 'gdi32', 'gdi', 'ole32', 'oleaut32', 'advapi32', 'shell32',
    'shcore', 'comctl32', 'comdlg32', 'ws2_32', 'crypt32', 'version', 'winmm',
    'ntdll', 'msvcp140', 'msvcp140_1', 'msvcp140_2', 'vcruntime140', 'vcruntime140_1',
    'ucrtbase', 'bcrypt', 'psapi', 'setupapi', 'shlwapi', 'userenv',
    'secur32', 'dbghelp', 'cfgmgr32', 'powrprof', 'imm32', 'propsys',
    'd3d9', 'd3d10', 'd3d11', 'd3d12', 'dxgi', 'dxva2', 'opengl32',
    'mfplat', 'mf', 'mfreadwrite', 'evr', 'd2d1', 'directwrite',
    'rpcrt4', 'sechost', 'combase', 'ncrypt', 'wintrust', 'wer',
    'bcryptprimitives', 'dxgidebug',
    'uxtheme', 'dwmapi', 'uiautomationcore', 'magnification',
    'libegl', 'libglesv2', 'd3dcompiler_47',
    # GPU driver DLLs
    'atioglxx', 'atiogl', 'nvoglv64', 'nvoglv32', 'igdgmm64', 'igdumdim64',
    # Substring noise from version strings, format strings, etc.
    'darkmode_explorerntdll', 'lwgl_arb_create_context_no_erroropengl32'
)

$shippedNames = @{}
foreach ($f in $items) { $shippedNames[$f.Name.ToLower()] = $true }
$expectedLookup = @{}
foreach ($n in $expectedSystem) { $expectedLookup["$n.dll"] = $true }

$rx = [System.Text.RegularExpressions.Regex]::new('([a-z0-9_+\-]{2,40}\.dll)', 'IgnoreCase')
$unresolved = New-Object System.Collections.Generic.HashSet[string]
foreach ($f in $items) {
    if ($f.Extension -ne '.dll' -and $f.Extension -ne '.exe') { continue }
    $bytes = [System.IO.File]::ReadAllBytes($f.FullName)
    $ascii = [System.Text.Encoding]::ASCII.GetString($bytes)
    foreach ($m in $rx.Matches($ascii)) {
        $name = $m.Value.ToLower()
        if ($name -eq $f.Name.ToLower()) { continue }
        if ($shippedNames.ContainsKey($name)) { continue }
        if ($expectedLookup.ContainsKey($name)) { continue }
        # Strip api-ms-win-* placeholders that resolve via Windows itself.
        if ($name -like 'api-ms-win-*') { continue }
        if ($name -like 'ext-ms-*') { continue }
        [void]$unresolved.Add("$($f.Name) -> $name")
    }
}

if ($unresolved.Count -gt 0) {
    Write-Host ""
    Write-Host "WARNING: shipped binaries reference these DLLs that are not in the bundle and not in the system-DLL allowlist:" -ForegroundColor Yellow
    foreach ($u in ($unresolved | Sort-Object)) {
        Write-Host "  $u" -ForegroundColor Yellow
    }
    Write-Host "If those are real load-time imports, the portable will fail on machines that don't already have them. Add them to `$requiredDlls or `$expectedSystem in scripts\package-portable.ps1." -ForegroundColor Yellow
}

Write-Host ""
Write-Host "Run with:"
Write-Host ("  & '{0}' <folder-with-photos>" -f (Join-Path $outDir "iv.exe"))
