<#
.SYNOPSIS
    Packages Loom for Windows into a Portable ZIP archive, an optional WiX MSI installer,
    and a SHA256SUMS.txt manifest, with conditional Authenticode code signing.

.DESCRIPTION
    Supports conditional Authenticode signing via signtool.exe when either:
      - LOOM_SIGN_PFX_BASE64 + LOOM_SIGN_PFX_PASSWORD environment variables are set, or
      - LOOM_SIGN_CERT_PATH (and optional LOOM_SIGN_PFX_PASSWORD) is set.
    When signing variables are unset, signing is cleanly skipped with an informational log.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Version,

    [ValidateSet('x64', 'arm64', 'x86')]
    [string]$Arch = 'x64',

    [string]$BinaryPath = '',

    [string]$OutputDir = '',

    [switch]$SkipMsi,

    [string]$TimestampUrl = 'http://timestamp.digicert.com'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Resolve-SignToolPath {
    $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($cmd) {
        return $cmd.Source
    }

    $sdkRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    if (Test-Path -LiteralPath $sdkRoot) {
        $candidates = Get-ChildItem -Path $sdkRoot -Filter 'signtool.exe' -Recurse -ErrorAction SilentlyContinue |
            Where-Object { $_.Directory.Name -eq 'x64' } |
            Sort-Object FullName -Descending
        if ($candidates) {
            return $candidates[0].FullName
        }
    }

    throw "Authenticode signing is enabled, but 'signtool.exe' was not found in PATH or Windows 10 SDK."
}

function Invoke-AuthenticodeSign {
    param(
        [Parameter(Mandatory = $true)][string]$SignToolPath,
        [Parameter(Mandatory = $true)][string]$CertPath,
        [string]$CertPassword,
        [Parameter(Mandatory = $true)][string]$TimestampServer,
        [Parameter(Mandatory = $true)][string]$TargetPath
    )

    Write-Host "[package-release] Signing $(Split-Path -Leaf $TargetPath)..."
    $args = @(
        'sign',
        '/fd', 'SHA256',
        '/tr', $TimestampServer,
        '/td', 'SHA256',
        '/f', $CertPath
    )
    if (-not [string]::IsNullOrEmpty($CertPassword)) {
        $args += @('/p', $CertPassword)
    }
    $args += $TargetPath

    & $SignToolPath @args
    if ($LASTEXITCODE -ne 0) {
        throw "signtool.exe failed with exit code $LASTEXITCODE while signing '$TargetPath'."
    }
}

function Remove-FileSecurely {
    param([string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path) -or -not (Test-Path -LiteralPath $Path)) {
        return
    }

    try {
        $length = (Get-Item -LiteralPath $Path).Length
        if ($length -gt 0) {
            $zeros = New-Object byte[] $length
            [System.IO.File]::WriteAllBytes($Path, $zeros)
        }
    }
    finally {
        Remove-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    }
}

$normalizedVersion = $Version.Trim().TrimStart('v', 'V')
if ($normalizedVersion -notmatch '^\d+\.\d+\.\d+([\-+][0-9A-Za-z.\-]+)?$') {
    throw "Invalid semantic version '$Version'. Expected format like '0.1.0' or '0.1.0-rc.1'."
}
$msiVersion = ($normalizedVersion -split '[\-+]')[0]

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

if ([string]::IsNullOrWhiteSpace($BinaryPath)) {
    $targetBase = if (-not [string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
        $env:CARGO_TARGET_DIR
    } else {
        Join-Path $repoRoot 'target'
    }
    $BinaryPath = Join-Path $targetBase 'release\loom-app.exe'
}

if (-not (Test-Path -LiteralPath $BinaryPath)) {
    throw "Release binary not found at '$BinaryPath'. Build it first with 'cargo build --locked --release -p loom-app'."
}
$resolvedBinaryPath = (Resolve-Path -LiteralPath $BinaryPath).Path

if ([string]::IsNullOrWhiteSpace($OutputDir)) {
    $OutputDir = Join-Path $repoRoot 'dist'
}
New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null
$resolvedOutputDir = (Resolve-Path -LiteralPath $OutputDir).Path

$licensePath = Join-Path $repoRoot 'LICENSE'
$readmePath = Join-Path $repoRoot 'README.md'
$changelogPath = Join-Path $repoRoot 'CHANGELOG.md'
$wxsPath = Join-Path $PSScriptRoot 'wix\main.wxs'

foreach ($requiredFile in @($licensePath, $readmePath, $changelogPath)) {
    if (-not (Test-Path -LiteralPath $requiredFile)) {
        throw "Required repository file not found: '$requiredFile'."
    }
}

$tempPfxPath = $null
$stagingDir = Join-Path ([System.IO.Path]::GetTempPath()) ("loom-release-stage-" + [guid]::NewGuid().ToString('N'))

try {
    $signingEnabled = $false
    $signToolPath = $null
    $certPath = $null
    $certPassword = $env:LOOM_SIGN_PFX_PASSWORD

    if (-not [string]::IsNullOrWhiteSpace($env:LOOM_SIGN_PFX_BASE64)) {
        if ([string]::IsNullOrWhiteSpace($certPassword)) {
            throw "LOOM_SIGN_PFX_BASE64 is set, but LOOM_SIGN_PFX_PASSWORD is empty."
        }
        $tempPfxPath = Join-Path ([System.IO.Path]::GetTempPath()) ("loom-sign-" + [guid]::NewGuid().ToString('N') + '.pfx')
        $pfxBytes = [System.Convert]::FromBase64String($env:LOOM_SIGN_PFX_BASE64.Trim())
        [System.IO.File]::WriteAllBytes($tempPfxPath, $pfxBytes)
        [Array]::Clear($pfxBytes, 0, $pfxBytes.Length)
        $certPath = $tempPfxPath
        $signToolPath = Resolve-SignToolPath
        $signingEnabled = $true
        Write-Host "[package-release] Authenticode signing enabled via LOOM_SIGN_PFX_BASE64."
    }
    elseif (-not [string]::IsNullOrWhiteSpace($env:LOOM_SIGN_CERT_PATH)) {
        if (-not (Test-Path -LiteralPath $env:LOOM_SIGN_CERT_PATH)) {
            throw "LOOM_SIGN_CERT_PATH points to a non-existent file: '$($env:LOOM_SIGN_CERT_PATH)'."
        }
        $certPath = (Resolve-Path -LiteralPath $env:LOOM_SIGN_CERT_PATH).Path
        $signToolPath = Resolve-SignToolPath
        $signingEnabled = $true
        Write-Host "[package-release] Authenticode signing enabled via LOOM_SIGN_CERT_PATH."
    }
    else {
        Write-Host "[package-release] Authenticode signing skipped (LOOM_SIGN_PFX_BASE64 / LOOM_SIGN_CERT_PATH not configured)."
    }

    New-Item -ItemType Directory -Path $stagingDir -Force | Out-Null

    $stagedExe = Join-Path $stagingDir 'loom-app.exe'
    Copy-Item -LiteralPath $resolvedBinaryPath -Destination $stagedExe -Force
    if ($signingEnabled) {
        Invoke-AuthenticodeSign `
            -SignToolPath $signToolPath `
            -CertPath $certPath `
            -CertPassword $certPassword `
            -TimestampServer $TimestampUrl `
            -TargetPath $stagedExe
    }

    Copy-Item -LiteralPath $licensePath -Destination (Join-Path $stagingDir 'LICENSE') -Force
    Copy-Item -LiteralPath $readmePath -Destination (Join-Path $stagingDir 'README.md') -Force
    Copy-Item -LiteralPath $changelogPath -Destination (Join-Path $stagingDir 'CHANGELOG.md') -Force

    $emittedArtifacts = New-Object System.Collections.Generic.List[string]

    # 1. Portable ZIP package
    $zipName = "loom-$normalizedVersion-windows-$Arch-portable.zip"
    $zipPath = Join-Path $resolvedOutputDir $zipName
    if (Test-Path -LiteralPath $zipPath) {
        Remove-Item -LiteralPath $zipPath -Force
    }
    Write-Host "[package-release] Creating portable ZIP: $zipName"
    Compress-Archive -Path (Join-Path $stagingDir '*') -DestinationPath $zipPath -CompressionLevel Optimal -Force
    $emittedArtifacts.Add($zipPath)

    # 2. WiX MSI installer
    if ($SkipMsi) {
        Write-Host "[package-release] Skipping MSI build (-SkipMsi specified)."
    }
    else {
        $wixCmd = Get-Command wix -ErrorAction SilentlyContinue
        if (-not $wixCmd) {
            Write-Warning "[package-release] WiX CLI ('wix') not found in PATH; skipping MSI build. Install with 'dotnet tool install --global wix' or pass -SkipMsi."
        }
        else {
            if (-not (Test-Path -LiteralPath $wxsPath)) {
                throw "WiX source file not found at '$wxsPath'."
            }
            $msiName = "loom-$normalizedVersion-windows-$Arch.msi"
            $msiPath = Join-Path $resolvedOutputDir $msiName
            if (Test-Path -LiteralPath $msiPath) {
                Remove-Item -LiteralPath $msiPath -Force
            }

            Write-Host "[package-release] Building WiX MSI ($msiVersion, $Arch): $msiName"
            & $wixCmd.Source build $wxsPath `
                -arch $Arch `
                -d "Version=$msiVersion" `
                -d "BinaryPath=$stagedExe" `
                -d "LicensePath=$licensePath" `
                -o $msiPath
            if ($LASTEXITCODE -ne 0) {
                throw "wix build failed with exit code $LASTEXITCODE."
            }

            if ($signingEnabled) {
                Invoke-AuthenticodeSign `
                    -SignToolPath $signToolPath `
                    -CertPath $certPath `
                    -CertPassword $certPassword `
                    -TimestampServer $TimestampUrl `
                    -TargetPath $msiPath
            }
            $emittedArtifacts.Add($msiPath)
        }
    }

    # 3. SHA256SUMS.txt manifest
    $checksumLines = New-Object System.Collections.Generic.List[string]
    foreach ($artifactPath in ($emittedArtifacts | Sort-Object { Split-Path -Leaf $_ })) {
        $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $artifactPath).Hash.ToLowerInvariant()
        $fileName = Split-Path -Leaf $artifactPath
        $checksumLines.Add("$hash  $fileName")
    }

    $sumsPath = Join-Path $resolvedOutputDir 'SHA256SUMS.txt'
    $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($sumsPath, (($checksumLines -join "`n") + "`n"), $utf8NoBom)
    Write-Host "[package-release] Wrote checksums: $sumsPath"
}
finally {
    Remove-FileSecurely -Path $tempPfxPath
    if (Test-Path -LiteralPath $stagingDir) {
        Remove-Item -LiteralPath $stagingDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
