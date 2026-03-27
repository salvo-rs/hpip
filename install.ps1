#Requires -Version 5.1
<#
.SYNOPSIS
    Install hpip on Windows.
.DESCRIPTION
    Downloads the latest hpip release from GitHub and installs it.
.PARAMETER Version
    Specific version to install (e.g., "v0.1.2"). Defaults to latest.
.PARAMETER InstallDir
    Installation directory. Defaults to "$env:USERPROFILE\.hpip\bin".
.EXAMPLE
    irm https://raw.githubusercontent.com/salvo-rs/hpip/main/install.ps1 | iex
.EXAMPLE
    .\install.ps1 -Version v0.1.2
#>

param(
    [string]$Version = "",
    [string]$InstallDir = ""
)

$ErrorActionPreference = "Stop"

$Repo = "salvo-rs/hpip"
$Binary = "hpip"

function Get-LatestVersion {
    $url = "https://api.github.com/repos/$Repo/releases/latest"
    try {
        $release = Invoke-RestMethod -Uri $url -Headers @{ "User-Agent" = "hpip-installer" }
        return $release.tag_name
    }
    catch {
        throw "Failed to fetch latest version: $_"
    }
}

function Get-InstallDirectory {
    if ($InstallDir -ne "") {
        return $InstallDir
    }
    $dir = Join-Path $env:USERPROFILE ".hpip\bin"
    return $dir
}

function Install-Hpip {
    $target = "x86_64-pc-windows-msvc"

    if ([string]::IsNullOrEmpty($Version)) {
        $Version = Get-LatestVersion
    }

    $installPath = Get-InstallDirectory

    if (-not (Test-Path $installPath)) {
        New-Item -ItemType Directory -Path $installPath -Force | Out-Null
    }

    $archiveName = "$Binary-$target.zip"
    $url = "https://github.com/$Repo/releases/download/$Version/$archiveName"

    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

    try {
        $archivePath = Join-Path $tmpDir $archiveName

        Write-Host "Downloading $Binary $Version for $target..."
        Invoke-WebRequest -Uri $url -OutFile $archivePath -UseBasicParsing

        Write-Host "Extracting to $installPath..."
        Expand-Archive -Path $archivePath -DestinationPath $tmpDir -Force

        $exePath = Join-Path $tmpDir "$Binary.exe"
        Copy-Item -Path $exePath -Destination (Join-Path $installPath "$Binary.exe") -Force
    }
    finally {
        Remove-Item -Path $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }

    # Add to user PATH if not already present
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -notlike "*$installPath*") {
        [Environment]::SetEnvironmentVariable("Path", "$installPath;$userPath", "User")
        $env:Path = "$installPath;$env:Path"
        Write-Host ""
        Write-Host "Added $installPath to your user PATH."
        Write-Host "Restart your terminal for the PATH change to take effect."
    }

    Write-Host ""
    Write-Host "Successfully installed $Binary $Version to $installPath\$Binary.exe"
}

Install-Hpip
