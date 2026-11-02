# Kern installer for Windows.
#
#   irm https://raw.githubusercontent.com/chitajohn/Kern/main/scripts/install.ps1 | iex
#
# Environment / parameters:
#   -Version      version to install (default: latest release, e.g. "0.1.0")
#   -InstallDir   install directory (default: %LOCALAPPDATA%\Kern\bin)
#   $env:KERN_REPO  owner/repo override (default: chitajohn/Kern)
#   $env:KERN_BASE_URL  release host for mirrors (default: https://github.com/$KERN_REPO/releases)
param(
    [string]$Version = $env:KERN_VERSION,
    [string]$InstallDir = $env:KERN_INSTALL_DIR
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = if ($env:KERN_REPO) { $env:KERN_REPO } else { 'chitajohn/Kern' }
$bin = 'kern.exe'

if (-not $InstallDir) {
    $InstallDir = Join-Path $env:LOCALAPPDATA 'Kern\bin'
}

$arch = $env:PROCESSOR_ARCHITECTURE
switch ($arch) {
    'AMD64' { $target = 'x86_64-pc-windows-msvc' }
    'ARM64' {
        throw "No prebuilt binary for Windows on ARM yet. Install with Rust instead: cargo install --git https://github.com/$repo kern-runtime"
    }
    default {
        throw "Unsupported architecture '$arch'. Install with Rust instead: cargo install --git https://github.com/$repo kern-runtime"
    }
}

if (-not $Version) {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" -UseBasicParsing
    $Version = $release.tag_name
}
$Version = $Version -replace '^v', ''

$baseUrl = if ($env:KERN_BASE_URL) { $env:KERN_BASE_URL.TrimEnd('/') } else { "https://github.com/$repo/releases" }

$name = "kern-$Version-$target"
$archive = "$name.zip"
$url = "$baseUrl/download/v$Version/$archive"

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
    Write-Host "Downloading kern $Version for $target..."
    $zip = Join-Path $tmp $archive
    try {
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    } catch {
        throw "Download failed: $url`nNo prebuilt binary for this version/platform? Install with Rust instead: cargo install --git https://github.com/$repo kern-runtime"
    }

    $expected = $null
    try {
        $shaContent = (Invoke-WebRequest -Uri "$url.sha256" -UseBasicParsing -ErrorAction Stop).Content
        # Windows PowerShell 5.1 hands back a byte array when the response has no charset.
        if ($shaContent -is [byte[]]) {
            $shaContent = [System.Text.Encoding]::UTF8.GetString($shaContent)
        }
        $expected = $shaContent.ToString().Trim().Split(' ')[0].ToLower()
    } catch {
        # No checksum published for this asset; continue without verification.
    }
    if ($expected) {
        $actual = (Get-FileHash -Path $zip -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) { throw "Checksum mismatch for $archive" }
    }

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $exe = Get-ChildItem -Path $tmp -Filter $bin -Recurse | Select-Object -First 1
    if (-not $exe) { throw "kern.exe was not found inside $archive" }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item $exe.FullName (Join-Path $InstallDir 'kern.exe') -Force
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Write-Host "Installed $(Join-Path $InstallDir 'kern.exe')"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath) { $userPath = '' }
$entries = $userPath.Split(';') | Where-Object { $_ }
if ($entries -notcontains $InstallDir) {
    $newPath = (@($entries) + $InstallDir) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host "Added $InstallDir to your user PATH. Restart your terminal to use 'kern'."
} else {
    & (Join-Path $InstallDir 'kern.exe') --version
}
