param(
    [Parameter(Mandatory)][string]$ReleasesUrl
)
$ErrorActionPreference = 'Stop'
if (-not $ReleasesUrl.StartsWith('https://')) { throw 'Release feed must use HTTPS' }
$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    cargo build --release --locked -p kratos
    if ($LASTEXITCODE -ne 0) { throw 'Windows build failed' }

    $tailcatOut = Join-Path $root 'target/tailcat'
    New-Item -ItemType Directory -Force -Path $tailcatOut | Out-Null
    Push-Location (Join-Path $root 'connectivity/tailcat')
    try {

        python ./licenses/generate.py --check
        if ($LASTEXITCODE -ne 0) { throw 'Tailcat license bundle is stale' }
        $env:CGO_ENABLED = '0'
        go build -trimpath -ldflags '-s -w' -o (Join-Path $tailcatOut 'kratos-tailcat.exe') ./cmd/kratos-tailcat
        if ($LASTEXITCODE -ne 0) { throw 'Tailcat companion build failed' }
    } finally { Pop-Location }
    # Pipe GUI-subsystem executables so PowerShell waits for their output.
    $versionText = & ./target/release/kratos.exe --version | Out-String
    $versionMatch = [regex]::Match($versionText.Trim(), '^kratos (\d+\.\d+\.\d+)$')
    if ($LASTEXITCODE -ne 0 -or -not $versionMatch.Success) { throw "Cannot read executable version: $versionText" }
    $version = $versionMatch.Groups[1].Value
    $out = Join-Path $root 'target/package'
    $stage = Join-Path $out "kratos-$version-windows-x86_64"
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $stage
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    Copy-Item -LiteralPath './target/release/kratos.exe' -Destination (Join-Path $stage 'kratos.exe')

    $companion = Join-Path $stage 'kratos-tailcat.exe'
    Copy-Item -LiteralPath './target/tailcat/kratos-tailcat.exe' -Destination $companion
    $companionHash = (Get-FileHash -LiteralPath $companion -Algorithm SHA256).Hash.ToLowerInvariant()
    # The updater validates this digest after safely extracting the complete ZIP.
    @{ releases_url = $ReleasesUrl; companion = @{ file = 'kratos-tailcat.exe'; sha256 = $companionHash } } |
        ConvertTo-Json -Depth 3 | Set-Content -Encoding utf8NoBOM -LiteralPath (Join-Path $stage 'kratos-update.json')
    Copy-Item -LiteralPath 'LICENSE','THIRD_PARTY_NOTICES.md' -Destination $stage
    $licenses = Join-Path $stage 'licenses/fonts'
    New-Item -ItemType Directory -Force -Path $licenses | Out-Null
    Copy-Item -Path 'crates/ui/assets/fonts/licenses/*' -Destination $licenses

    $tailcatLicenses = Join-Path $stage 'licenses/tailcat'
    New-Item -ItemType Directory -Force -Path $tailcatLicenses | Out-Null
    Copy-Item -Recurse -Path 'connectivity/tailcat/licenses/bundle/*' -Destination $tailcatLicenses
    Compress-Archive -Path "$stage/*" -DestinationPath "$stage.zip" -Force
    # Retained only as a legacy app-only artifact; updater and fresh installs use ZIP.
    Copy-Item -LiteralPath './target/release/kratos.exe' -Destination "$stage.exe"
    $zipFile = Split-Path "$stage.zip" -Leaf
    $zipHash = (Get-FileHash -LiteralPath "$stage.zip" -Algorithm SHA256).Hash.ToLowerInvariant()
    @{ version = $version; files = @{ $zipFile = @{ sha256 = $zipHash } } } |
        ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8NoBOM -LiteralPath (Join-Path $out 'manifest.json')
    Write-Host "Packaged $stage.zip"
} finally { Pop-Location }
