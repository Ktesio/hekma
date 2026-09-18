$ErrorActionPreference = "Stop"

# Hekma installer (a Ktesio project). Installs the `hekma` + `hkm`
# binaries; detects an existing Hekma OR legacy `kt` install and migrates
# it along its original install channel. A retired `kt` is never deleted
# silently — a visible note names it.
$Repo = "Ktesio/hekma"
$Crate = "hekma"
$Bin = "hekma.exe"
$Hkm = "hkm.exe"
$LegacyBin = "kt.exe"
$LatestReleaseUrl = "https://api.github.com/repos/$Repo/releases/latest"
$ReleaseBaseUrl = "https://github.com/$Repo/releases/download"

function First-Env([string]$Forward, [string]$Legacy) {
    if ($null -ne (Get-Item Env:$Forward -ErrorAction SilentlyContinue) -and
        -not [string]::IsNullOrWhiteSpace((Get-Item Env:$Forward).Value)) {
        return (Get-Item Env:$Forward).Value
    }
    if ($null -ne (Get-Item Env:$Legacy -ErrorAction SilentlyContinue) -and
        -not [string]::IsNullOrWhiteSpace((Get-Item Env:$Legacy).Value)) {
        return (Get-Item Env:$Legacy).Value
    }
    return $null
}

$Method = First-Env "HEKMA_INSTALL_METHOD" "KTESIO_INSTALL_METHOD"
if (-not $Method) { $Method = "auto" }

function Write-Info($Message) {
    Write-Host $Message
}

function Write-WarningMessage($Message) {
    Write-Warning $Message
}

function Fail($Message) {
    throw $Message
}

function Test-Truthy($Value) {
    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $false
    }

    return $Value -notmatch '^(0|false|no|off)$'
}

function Test-DryRun {
    $dry = First-Env "HEKMA_INSTALL_DRY_RUN" "KTESIO_INSTALL_DRY_RUN"
    return Test-Truthy $dry
}

function Test-Command($Name) {
    if ($Name -eq "cargo" -and $null -ne $env:KTESIO_INSTALL_TEST_HAS_CARGO) {
        return $env:KTESIO_INSTALL_TEST_HAS_CARGO -eq "1"
    }

    return $null -ne (Get-Command $Name -ErrorAction SilentlyContinue)
}

function Find-ExistingBinary {
    if ($null -ne $env:KTESIO_INSTALL_TEST_KT_PATH) {
        if ($env:KTESIO_INSTALL_TEST_KT_PATH.Length -gt 0) {
            return $env:KTESIO_INSTALL_TEST_KT_PATH
        }
        return $null
    }

    # hekma first, then the retired kt (either name, with or without .exe).
    foreach ($name in @("hekma.exe", "hekma", "kt.exe", "kt")) {
        $command = Get-Command $name -ErrorAction SilentlyContinue
        if ($null -ne $command) {
            return $command.Source
        }
    }
    return $null
}

function Test-HekmaBinary($Path) {
    if (-not $Path -or -not (Test-Path -LiteralPath $Path)) {
        return $false
    }

    try {
        $output = & $Path --version 2>$null
        # Both shipped binaries report the shared identity: "hekma <v>".
        return "$output" -match '^hekma v?[0-9]'
    }
    catch {
        return $false
    }
}

function Test-LegacyKtBinary($Path) {
    if (-not $Path -or -not (Test-Path -LiteralPath $Path)) {
        return $false
    }

    try {
        $output = & $Path --version 2>$null
        return "$output" -match '^kt v?[0-9]'
    }
    catch {
        return $false
    }
}

function Test-OwnedBinary($Path) {
    return (Test-HekmaBinary $Path) -or (Test-LegacyKtBinary $Path)
}

function Get-ExistingMethod($Path) {
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE ".cargo" }
    $cargoBin = Join-Path $cargoHome "bin"

    if ($Path.StartsWith($cargoBin, [System.StringComparison]::OrdinalIgnoreCase)) {
        return "cargo"
    }

    return "manual"
}

function Invoke-OrDryRun($Command, [string[]]$Arguments) {
    if (Test-DryRun) {
        Write-Info "DRY RUN: $Command $($Arguments -join ' ')"
        return
    }

    & $Command @Arguments
}

function Install-WithCargo {
    if (-not (Test-Command "cargo")) {
        Fail "Cargo is not available on PATH."
    }

    Invoke-OrDryRun "cargo" @("install", $Crate, "--force")
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE ".cargo" }
    Note-RetiredKt (Join-Path (Join-Path $cargoHome "bin") $LegacyBin)
}

function Get-DefaultInstallDir {
    $override = First-Env "HEKMA_INSTALL_DIR" "KTESIO_INSTALL_DIR"
    if ($override) {
        return $override
    }

    if ($env:LOCALAPPDATA) {
        return Join-Path $env:LOCALAPPDATA "hekma\bin"
    }

    if ($env:USERPROFILE) {
        return Join-Path $env:USERPROFILE ".hekma\bin"
    }

    Fail "HEKMA_INSTALL_DIR (or KTESIO_INSTALL_DIR) is required when LOCALAPPDATA and USERPROFILE are not set."
}

function Test-DirOnPath($Dir) {
    $parts = ($env:PATH -split ';') | Where-Object { $_ }
    return $parts -contains $Dir
}

function Note-RetiredKt($RetiredPath) {
    if ((Test-Path -LiteralPath $RetiredPath) -and (Test-LegacyKtBinary $RetiredPath)) {
        Write-WarningMessage "the retired kt binary was left at $RetiredPath - Hekma 0.8.0 replaced it with hekma + hkm; remove it manually when ready"
    }
}

function Get-InstallTarget {
    param($ExistingPath)

    $installDir = First-Env "HEKMA_INSTALL_DIR" "KTESIO_INSTALL_DIR"
    if (-not $installDir) {
        if ($ExistingPath) {
            # Reuse the legacy install directory (e.g. ...\ktesio\bin) so
            # hekma lands beside the retired kt instead of in a second
            # location on PATH.
            $installDir = Split-Path -Parent $ExistingPath
        }
        else {
            $installDir = Get-DefaultInstallDir
        }
    }

    if (Test-Path -LiteralPath $installDir) {
        $item = Get-Item -LiteralPath $installDir
        if (-not $item.PSIsContainer) {
            Fail "$installDir exists but is not a directory."
        }
    }
    elseif (-not (Test-DryRun)) {
        New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    }

    foreach ($name in @($Bin, $Hkm, $LegacyBin)) {
        $target = Join-Path $installDir $name
        if ((Test-Path -LiteralPath $target) -and -not (Test-OwnedBinary $target)) {
            Fail "Refusing to overwrite non-Ktesio executable at $target."
        }
    }

    return $installDir
}

function Get-LatestReleaseTag {
    $release = Invoke-RestMethod -Uri $LatestReleaseUrl -Headers @{ "User-Agent" = "hekma-installer" }
    if (-not $release.tag_name) {
        Fail "Could not resolve the latest Hekma release tag from GitHub."
    }

    return $release.tag_name
}

function Install-WithBinary($ExistingPath) {
    $installDir = Get-InstallTarget $ExistingPath

    $arch = if ($env:KTESIO_INSTALL_TEST_ARCH) { $env:KTESIO_INSTALL_TEST_ARCH } else { $env:PROCESSOR_ARCHITECTURE }
    if ($arch -notin @("AMD64", "x86_64")) {
        Fail "No prebuilt Hekma binary is available for Windows/$arch. Install Rust and run: cargo install hekma --force"
    }

    $target = "x86_64-pc-windows-msvc"
    if (Test-DryRun) {
        Write-Info "DRY RUN: install prebuilt $target ($Bin + $Hkm) to $installDir"
        if (-not (Test-DirOnPath $installDir)) {
            Write-WarningMessage "$installDir is not on PATH. Add it before running hekma."
        }
        return
    }

    $tag = Get-LatestReleaseTag
    $asset = "hekma-$tag-$target.zip"
    $assetUrl = "$ReleaseBaseUrl/$tag/$asset"
    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null

    try {
        $archive = Join-Path $tmpDir $asset
        $checksumFile = Join-Path $tmpDir "$asset.sha256"
        $packageDir = Join-Path $tmpDir "package"

        Write-Info "Downloading Hekma $tag for $target..."
        Invoke-WebRequest -Uri $assetUrl -OutFile $archive
        Invoke-WebRequest -Uri "$assetUrl.sha256" -OutFile $checksumFile

        $expected = ((Get-Content -LiteralPath $checksumFile -Raw) -split '\s+')[0].ToLowerInvariant()
        $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($expected -ne $actual) {
            Fail "Checksum verification failed for $asset."
        }

        Expand-Archive -LiteralPath $archive -DestinationPath $packageDir -Force
        foreach ($name in @($Bin, $Hkm)) {
            $binary = Get-ChildItem -LiteralPath $packageDir -Recurse -File -Filter $name | Select-Object -First 1
            if ($null -eq $binary) {
                Fail "Release archive did not contain $name."
            }
            Copy-Item -LiteralPath $binary.FullName -Destination (Join-Path $installDir $name) -Force
        }

        Write-Info "Installed Hekma to $(Join-Path $installDir $Bin) and $(Join-Path $installDir $Hkm)"
        Note-RetiredKt (Join-Path $installDir $LegacyBin)
        if (-not (Test-DirOnPath $installDir)) {
            Write-WarningMessage "$installDir is not on PATH. Add it before running hekma."
        }
        & (Join-Path $installDir $Bin) --version
    }
    finally {
        Remove-Item -LiteralPath $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Install-Auto {
    $existing = Find-ExistingBinary
    if ($existing) {
        if (-not (Test-OwnedBinary $existing)) {
            Fail "Refusing to overwrite non-Ktesio command at $existing."
        }

        $existingMethod = Get-ExistingMethod $existing
        if ($existingMethod -eq "cargo") {
            Install-WithCargo
        }
        else {
            Install-WithBinary $existing
        }
        return
    }

    if (Test-Command "cargo") {
        Install-WithCargo
        return
    }

    Install-WithBinary $null
}

if ($Method -notin @("auto", "cargo", "binary")) {
    Fail "HEKMA_INSTALL_METHOD (or KTESIO_INSTALL_METHOD) must be one of: auto, cargo, binary."
}

$existingBinary = Find-ExistingBinary
if ($existingBinary -and -not (Test-OwnedBinary $existingBinary)) {
    Fail "Refusing to overwrite non-Ktesio command at $existingBinary."
}

switch ($Method) {
    "auto" { Install-Auto }
    "cargo" { Install-WithCargo }
    "binary" {
        if ($existingBinary -and (Get-ExistingMethod $existingBinary) -eq "manual") {
            Install-WithBinary $existingBinary
        }
        else {
            Install-WithBinary $null
        }
    }
}
