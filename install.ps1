# Install ytmx on Windows (PowerShell):
#   irm https://raw.githubusercontent.com/MaheshBhushan/ytmx/main/install.ps1 | iex
# Downloads the Windows build, checks its SHA-256, installs to
# %LOCALAPPDATA%\Programs\ytmx and adds that to your user PATH.
# $env:YTMX_VERSION = "v0.2.0" pins a release.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'MaheshBhushan/ytmx'
# ARM64 Windows runs the x64 build through its built-in emulation.
$target = 'x86_64-pc-windows-msvc'
$asset = "ytmx-$target.zip"
$base = if ($env:YTMX_VERSION) { "https://github.com/$repo/releases/download/$($env:YTMX_VERSION)" } else { "https://github.com/$repo/releases/latest/download" }
$dir = if ($env:YTMX_INSTALL_DIR) { $env:YTMX_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\ytmx' }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("ytmx-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "Downloading $asset..."
    Invoke-WebRequest "$base/$asset" -OutFile "$tmp\$asset" -UseBasicParsing
    $got = (Get-FileHash "$tmp\$asset" -Algorithm SHA256).Hash.ToLower()
    # Release assets are served as bytes, so read the checksum from a file rather than .Content.
    Invoke-WebRequest "$base/$asset.sha256" -OutFile "$tmp\sum" -UseBasicParsing
    $want = (Get-Content "$tmp\sum" -Raw).Trim().ToLower()
    if ($got -ne $want) { throw "checksum mismatch for $asset" }

    Expand-Archive "$tmp\$asset" -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    # A running player keeps ytmx.exe locked; stop it first.
    if (Test-Path "$dir\ytmx.exe") { & "$dir\ytmx.exe" quit 2>$null | Out-Null }
    Copy-Item "$tmp\ytmx.exe" "$dir\ytmx.exe" -Force
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $dir) {
    [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ";$dir"), 'User')
    $env:Path += ";$dir"
    Write-Host "Added $dir to your user PATH (new terminals pick it up)."
}
Write-Host "Installed ytmx to $dir\ytmx.exe ($(& "$dir\ytmx.exe" --version))"

$missing = @()
if (-not (Get-Command mpv -ErrorAction SilentlyContinue)) { $missing += 'mpv' }
if (-not (Get-Command yt-dlp -ErrorAction SilentlyContinue)) { $missing += 'yt-dlp' }
if ($missing.Count -gt 0) {
    Write-Host ""
    Write-Host "ytmx also needs: $($missing -join ', ')"
    Write-Host "  scoop install $($missing -join ' ')      (https://scoop.sh)"
    Write-Host "  or: winget install yt-dlp.yt-dlp, and mpv from https://mpv.io/installation/"
}
Write-Host ""
Write-Host "Next: ytmx login    (see the README for the omp browser relay)"
