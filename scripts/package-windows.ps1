<#
.SYNOPSIS
Package the release `convert` example as a standalone Windows zip.

.DESCRIPTION
Builds examples/convert.rs in release mode and stages it as anydoc.exe next to
the README and LICENSE, then writes
<OutDir>/anydoc-<version>-x86_64-pc-windows-msvc.zip. The version comes from the
root Cargo.toml, so it always matches the crate being packaged.

.PARAMETER OutDir
Directory to write the zip into. Defaults to dist/ at the repo root.

.PARAMETER SkipBuild
Reuse the existing target/release/examples/convert.exe instead of rebuilding.

.EXAMPLE
pwsh -File scripts/package-windows.ps1
#>
[CmdletBinding()]
param(
    [string]$OutDir,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $repoRoot 'dist' }

# The first `version = "..."` in the root manifest is the [package] one: the
# file opens with [workspace], which has no version key.
$manifest = Join-Path $repoRoot 'Cargo.toml'
$match = Select-String -Path $manifest -Pattern '^version = "([^"]+)"' | Select-Object -First 1
if (-not $match) { throw "no version found in $manifest" }
$version = $match.Matches[0].Groups[1].Value

$exe = Join-Path $repoRoot 'target\release\examples\convert.exe'
if (-not $SkipBuild) {
    Write-Host "Building anydoc $version (release)..."
    & cargo build --release --locked --manifest-path $manifest --example convert
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
}
if (-not (Test-Path $exe)) { throw "missing $exe (run without -SkipBuild)" }

$name = "anydoc-$version-x86_64-pc-windows-msvc"
$stage = Join-Path $repoRoot "target\package\$name"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

# `convert.exe` is too generic a name to land on someone's PATH.
Copy-Item $exe (Join-Path $stage 'anydoc.exe')
Copy-Item (Join-Path $repoRoot 'README.md') $stage
Copy-Item (Join-Path $repoRoot 'LICENSE') $stage

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$zip = Join-Path $OutDir "$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip

$size = [math]::Round((Get-Item $zip).Length / 1MB, 2)
Write-Host "Wrote $zip ($size MB)"
