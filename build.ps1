# Builds the three programs for Windows and Linux into dist\ (release, one file each).
#
#   .\build.ps1                 both systems
#   .\build.ps1 -Target win     Windows only        .\build.ps1 -Target linux    Linux only
#
# Needs rustup with the targets x86_64-pc-windows-msvc and x86_64-unknown-linux-gnu
# (rustup target add ...).  The linker is rust-lld, which rustup brings: no Visual Studio,
# no Linux machine and no WSL are needed, the Linux programs link on Windows.
param([ValidateSet('all', 'win', 'linux')][string]$Target = 'all')

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$targets = @()
if ($Target -in 'all', 'win') { $targets += @{ triple = 'x86_64-pc-windows-msvc'; dir = 'windows-x86_64'; ext = '.exe' } }
if ($Target -in 'all', 'linux') { $targets += @{ triple = 'x86_64-unknown-linux-gnu'; dir = 'linux-x86_64'; ext = '' } }

# JFLib 0.2.1's build helper splits one linker argument of the Linux build at blanks, so a
# target folder below a path with a blank in it ("My Projects\...") does not link.  Until that
# is fixed there, such a checkout builds into a folder without blanks.
$targetDir = Join-Path $root 'target'
if ($targetDir -match ' ') {
    $targetDir = Join-Path $env:TEMP 'buildbalancer-target'
    if ($targetDir -match ' ') { throw "The project path and TEMP both contain blanks; set CARGO_TARGET_DIR to a folder without blanks and run cargo yourself (see README, Building)." }
    Write-Host "note: building in $targetDir (the project path contains a blank)"
}
$env:CARGO_TARGET_DIR = $targetDir

Push-Location $root
try {
    foreach ($t in $targets) {
        Write-Host "== $($t.triple)"
        # cargo reports its progress on standard error, which Windows PowerShell would take
        # for a failure: only its exit code counts
        $ErrorActionPreference = 'Continue'
        cargo build --release --target $t.triple
        $ErrorActionPreference = 'Stop'
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed for $($t.triple)" }
        $out = Join-Path $root "dist\$($t.dir)"
        New-Item -ItemType Directory -Force $out | Out-Null
        foreach ($name in 'bb', 'bb-relay', 'bb-worker') {
            Copy-Item (Join-Path $targetDir "$($t.triple)\release\$name$($t.ext)") $out -Force
        }
        Get-ChildItem $out | ForEach-Object { '{0,10:N0} KiB  {1}' -f ($_.Length / 1KB), $_.FullName }
    }
}
finally {
    Pop-Location
    Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
}
