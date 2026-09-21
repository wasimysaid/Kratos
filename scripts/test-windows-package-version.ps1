# Exercise the packaging script's actual version capture/validation statements.
param([string]$SourcePath = (Join-Path $PSScriptRoot 'package-windows.ps1'))
$ErrorActionPreference = 'Stop'
$source = Get-Content -Raw -LiteralPath $SourcePath
$start = $source.IndexOf('    $versionText =')
$end = $source.IndexOf('    $out =', $start)
if ($start -lt 0 -or $end -lt 0) { throw 'Cannot find packaging version statements' }
# Supply stdout fixtures in place of the native executable; retain the real pipeline and parser.
$statements = $source.Substring($start, $end - $start).Replace(
    '& ./target/release/kratos.exe --version', '& { $OutputLines }'
)
$parse = [scriptblock]::Create($statements + "`n" + '$version')
function Read-Version($OutputLines, $ExitCode) {
    $LASTEXITCODE = $ExitCode
    $Matches = $null
    & $parse
}
foreach ($lines in @(
    @{ output = 'kratos 0.2.66' },
    @{ output = @('kratos 0.2.66', '') },
    @{ output = @('', 'kratos 0.2.66', '') }
)) {
    $actual = Read-Version $lines.output 0
    if ($actual -cne '0.2.66') { throw "Wrong package version: $actual" }
}
foreach ($case in @(
    @{ output = ''; code = 0 },
    @{ output = 'not a version'; code = 0 },
    @{ output = 'kratos 0.2.66'; code = 1 },
    @{ output = @('kratos 0.2.66', 'unexpected output'); code = 0 }
)) {
    $rejected = $false
    try { Read-Version $case.output $case.code | Out-Null }
    catch { $rejected = $true }
    if (-not $rejected) { throw 'Invalid executable output was accepted' }
}
Write-Host 'Windows package version regression tests passed'
