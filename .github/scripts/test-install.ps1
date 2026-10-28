$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/../../install.ps1"
$fixtureRoot = [IO.Path]::GetFullPath((Join-Path ([IO.Path]::GetTempPath()) ('hpip-installer-test-' + [guid]::NewGuid())))
New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
try {
    $archive = Join-Path $fixtureRoot 'archive.zip'
    $checksums = Join-Path $fixtureRoot 'SHA256SUMS'
    [IO.File]::WriteAllText($archive, 'fixture')
    $digest = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
    Set-Content -LiteralPath $checksums -Value "$digest  archive.zip"
    Test-ArchiveChecksum $archive $checksums 'archive.zip'
    [IO.File]::WriteAllText($archive, 'corrupt')
    $rejected = $false
    try { Test-ArchiveChecksum $archive $checksums 'archive.zip' } catch { $rejected = $true }
    if (-not $rejected) { throw 'Corrupt archive unexpectedly passed verification' }
    $rejected = $false
    try { Test-ArchiveChecksum $archive $checksums 'missing.zip' } catch { $rejected = $true }
    if (-not $rejected) { throw 'Missing checksum unexpectedly passed verification' }
    Add-Content -LiteralPath $checksums -Value "$digest  archive.zip"
    $rejected = $false
    try { Test-ArchiveChecksum $archive $checksums 'archive.zip' } catch { $rejected = $true }
    if (-not $rejected) { throw 'Duplicate checksum unexpectedly passed verification' }
    Write-Host 'Windows installer checks passed'
}
finally {
    $tempParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if ($fixtureRoot.StartsWith($tempParent, [StringComparison]::OrdinalIgnoreCase) -and
        [IO.Path]::GetFileName($fixtureRoot).StartsWith('hpip-installer-test-')) {
        Remove-Item -LiteralPath $fixtureRoot -Recurse -Force
    }
}
