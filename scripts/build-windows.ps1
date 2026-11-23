$ErrorActionPreference = 'Stop'
Push-Location (Join-Path $PSScriptRoot '..')
try {
    cargo build --locked --release -p windows-host
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
