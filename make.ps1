param(
    [ValidateSet('test','build-host','build-firmware','prod','run-host')]
    [string]$Target = 'test',
    [string]$Port = 'COM3'
)
$ErrorActionPreference = 'Stop'
$env:PATH = (Join-Path $env:USERPROFILE '.cargo\bin') + ';' + $env:PATH
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (-not (Test-Path -LiteralPath $cargoBin)) { throw 'Rust/Cargo is missing. Install Rust from rustup.rs.' }
function Run-Cargo([string[]]$CargoArgs) {
    & $cargoBin @CargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo failed: $($CargoArgs -join ' ')" }
}
function Check-EspToolchain {
    $rustup = Join-Path $env:USERPROFILE '.cargo\bin\rustup.exe'
    $installed = & $rustup toolchain list
    if ($LASTEXITCODE -ne 0 -or -not ($installed | Where-Object { $_ -match '^esp(?:\s|$)' })) {
        throw 'Xtensa toolchain missing. Install espup, then run espup install --std --targets esp32 --toolchain-version 1.97.0.0 --skip-version-parse.'
    }
}
switch ($Target) {
    'test' {
        Run-Cargo @('test','--workspace')
        Run-Cargo @('build','-p','codex-phone-host')
    }
    'build-host' { Run-Cargo @('build','-p','codex-phone-host','--release') }
    'build-firmware' {
        Check-EspToolchain
        Run-Cargo @('+esp','build','--manifest-path','firmware/Cargo.toml','--release')
    }
    'prod' {
        Run-Cargo @('build','-p','codex-phone-host','--release')
        Check-EspToolchain
        Run-Cargo @('+esp','build','--manifest-path','firmware/Cargo.toml','--release')
        $espflash = Join-Path $env:USERPROFILE '.cargo\bin\espflash.exe'
        if (-not (Test-Path -LiteralPath $espflash)) { throw 'espflash is missing. Install it with cargo install espflash.' }
        & $espflash flash --port $Port 'firmware/target/xtensa-esp32-espidf/release/codex-phone-firmware'
        if ($LASTEXITCODE -ne 0) { throw 'espflash failed' }
    }
    'run-host' { Run-Cargo @('run','-p','codex-phone-host','--release') }
}
