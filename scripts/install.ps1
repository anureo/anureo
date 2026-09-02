param(
    [switch]$Help,
    [string]$Version = $env:ANUREO_VERSION,
    [string]$InstallDir = $env:ANUREO_INSTALL_DIR,
    [string]$Repository = $env:ANUREO_REPO
)

$ErrorActionPreference = 'Stop'

if ($Help) {
    @'
Install anureo from GitHub Releases.

Environment variables:
  ANUREO_VERSION       Release tag without the leading v (default: latest)
  ANUREO_INSTALL_DIR   Installation directory (default: %LOCALAPPDATA%\anureo\bin)
  ANUREO_REPO          GitHub repository (default: anureo/anureo)
'@
    exit 0
}

if ([string]::IsNullOrWhiteSpace($Version)) { $Version = 'latest' }
if ([string]::IsNullOrWhiteSpace($Repository)) { $Repository = 'anureo/anureo' }
if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = Join-Path $env:LOCALAPPDATA 'anureo\bin'
}

$target = 'x86_64-pc-windows-msvc'
if ($Version -eq 'latest') {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repository/releases/latest"
    $tag = $release.tag_name
    $Version = $tag.TrimStart('v')
} else {
    $Version = $Version.TrimStart('v')
    $tag = "v$Version"
}

$archive = "anureo-$Version-$target.zip"
$url = "https://github.com/$Repository/releases/download/$tag/$archive"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("anureo-install-" + [guid]::NewGuid())
$archivePath = Join-Path $tempDir $archive

try {
    New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
    Write-Host "Downloading anureo $Version for $target..."
    Invoke-WebRequest -Uri $url -OutFile $archivePath -UseBasicParsing
    Expand-Archive -Path $archivePath -DestinationPath $tempDir -Force

    $binary = Join-Path $tempDir 'anureo.exe'
    if (-not (Test-Path -LiteralPath $binary)) {
        throw "release archive does not contain anureo.exe"
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item -LiteralPath $binary -Destination (Join-Path $InstallDir 'anureo.exe') -Force
    Write-Host "anureo installed to $(Join-Path $InstallDir 'anureo.exe')"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $pathEntries = @($userPath -split ';' | Where-Object { $_ })
    if ($pathEntries -notcontains $InstallDir) {
        [Environment]::SetEnvironmentVariable('Path', (($pathEntries + $InstallDir) -join ';'), 'User')
        Write-Host "Added $InstallDir to the user PATH. Open a new terminal to use anureo."
    }
} finally {
    if (Test-Path -LiteralPath $tempDir) {
        Remove-Item -LiteralPath $tempDir -Recurse -Force
    }
}
