$ErrorActionPreference = "Stop"

$Repo = "Ktesio/ktesio"
$Crate = "ktesio"
$Bin = "kt.exe"
$LatestReleaseUrl = "https://api.github.com/repos/$Repo/releases/latest"
$ReleaseBaseUrl = "https://github.com/$Repo/releases/download"
$Method = if ($env:KTESIO_INSTALL_METHOD) { $env:KTESIO_INSTALL_METHOD } else { "auto" }

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
    return Test-Truthy $env:KTESIO_INSTALL_DRY_RUN
}

function Test-Command($Name) {
    if ($Name -eq "cargo" -and $null -ne $env:KTESIO_INSTALL_TEST_HAS_CARGO) {
        return $env:KTESIO_INSTALL_TEST_HAS_CARGO -eq "1"
    }

    return $null -ne (Get-Command $Name -ErrorAction SilentlyContinue)
}

function Find-ExistingKt {
    if ($null -ne $env:KTESIO_INSTALL_TEST_KT_PATH) {
        if ($env:KTESIO_INSTALL_TEST_KT_PATH.Length -gt 0) {
            return $env:KTESIO_INSTALL_TEST_KT_PATH
        }
        return $null
    }

    $command = Get-Command "kt.exe" -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        $command = Get-Command "kt" -ErrorAction SilentlyContinue
    }
    if ($null -eq $command) {
        return $null
    }

    return $command.Source
}

function Test-KtesioBinary($Path) {
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
}

function Get-DefaultInstallDir {
    if ($env:KTESIO_INSTALL_DIR) {
        return $env:KTESIO_INSTALL_DIR
    }

    if ($env:LOCALAPPDATA) {
        return Join-Path $env:LOCALAPPDATA "ktesio\bin"
    }

    if ($env:USERPROFILE) {
        return Join-Path $env:USERPROFILE ".ktesio\bin"
    }

    Fail "KTESIO_INSTALL_DIR is required when LOCALAPPDATA and USERPROFILE are not set."
}

function Test-DirOnPath($Dir) {
    $parts = ($env:PATH -split ';') | Where-Object { $_ }
    return $parts -contains $Dir
}

function Get-InstallTarget($ExistingPath) {
    if ($env:KTESIO_INSTALL_DIR) {
        $installDir = $env:KTESIO_INSTALL_DIR
    }
    elseif ($ExistingPath) {
        $installDir = Split-Path -Parent $ExistingPath
    }
    else {
        $installDir = Get-DefaultInstallDir
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

    $target = Join-Path $installDir $Bin
    if ((Test-Path -LiteralPath $target) -and -not (Test-KtesioBinary $target)) {
        Fail "Refusing to overwrite non-Ktesio executable at $target."
    }

    return $target
}

function Get-LatestReleaseTag {
    $release = Invoke-RestMethod -Uri $LatestReleaseUrl -Headers @{ "User-Agent" = "ktesio-installer" }
    if (-not $release.tag_name) {
        Fail "Could not resolve the latest Ktesio release tag from GitHub."
    }

    return $release.tag_name
}

function Install-WithBinary($ExistingPath) {
    $targetPath = Get-InstallTarget $ExistingPath
    $installDir = Split-Path -Parent $targetPath

    $arch = if ($env:KTESIO_INSTALL_TEST_ARCH) { $env:KTESIO_INSTALL_TEST_ARCH } else { $env:PROCESSOR_ARCHITECTURE }
    if ($arch -notin @("AMD64", "x86_64")) {
        Fail "No prebuilt Ktesio binary is available for Windows/$arch. Install Rust and run: cargo install ktesio --force"
    }

    $target = "x86_64-pc-windows-msvc"
    if (Test-DryRun) {
        Write-Info "DRY RUN: install prebuilt $target to $targetPath"
        if (-not (Test-DirOnPath $installDir)) {
            Write-WarningMessage "$installDir is not on PATH. Add it before running kt."
        }
        return
    }

    $tag = Get-LatestReleaseTag
    $asset = "ktesio-$tag-$target.zip"
    $assetUrl = "$ReleaseBaseUrl/$tag/$asset"
    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null

    try {
        $archive = Join-Path $tmpDir $asset
        $checksumFile = Join-Path $tmpDir "$asset.sha256"
        $packageDir = Join-Path $tmpDir "package"

        Write-Info "Downloading Ktesio $tag for $target..."
        Invoke-WebRequest -Uri $assetUrl -OutFile $archive
        Invoke-WebRequest -Uri "$assetUrl.sha256" -OutFile $checksumFile

        $expected = ((Get-Content -LiteralPath $checksumFile -Raw) -split '\s+')[0].ToLowerInvariant()
        $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($expected -ne $actual) {
            Fail "Checksum verification failed for $asset."
        }

        Expand-Archive -LiteralPath $archive -DestinationPath $packageDir -Force
        $binary = Get-ChildItem -LiteralPath $packageDir -Recurse -File -Filter $Bin | Select-Object -First 1
        if ($null -eq $binary) {
            Fail "Release archive did not contain $Bin."
        }

        Copy-Item -LiteralPath $binary.FullName -Destination $targetPath -Force
        Write-Info "Installed Ktesio to $targetPath"
        if (-not (Test-DirOnPath $installDir)) {
            Write-WarningMessage "$installDir is not on PATH. Add it before running kt."
        }
        & $targetPath --version
    }
    finally {
        Remove-Item -LiteralPath $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Install-Auto {
    $existing = Find-ExistingKt
    if ($existing) {
        if (-not (Test-KtesioBinary $existing)) {
            Fail "Refusing to overwrite non-Ktesio kt command at $existing."
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
    Fail "KTESIO_INSTALL_METHOD must be one of: auto, cargo, binary."
}

$existingKt = Find-ExistingKt
if ($existingKt -and -not (Test-KtesioBinary $existingKt)) {
    Fail "Refusing to overwrite non-Ktesio kt command at $existingKt."
}

switch ($Method) {
    "auto" { Install-Auto }
    "cargo" { Install-WithCargo }
    "binary" {
        if ($existingKt -and (Get-ExistingMethod $existingKt) -eq "manual") {
            Install-WithBinary $existingKt
        }
        else {
            Install-WithBinary $null
        }
    }
}
