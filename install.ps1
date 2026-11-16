# Build Balancer for Windows: fetch the programs for this machine and, if asked, set the machine
# up in the same go.
#
#   irm https://github.com/J4FF/Build-Balancer/releases/latest/download/install.ps1 | iex
#       only fetch the programs (bb, bb-worker, bb-relay, BuildBalancer)
#   & ([scriptblock]::Create((irm https://github.com/J4FF/Build-Balancer/releases/latest/download/install.ps1))) worker RELAY CODE
#       a machine that lends its power: pair with the code of the main machine, start lending
#   & ([scriptblock]::Create((irm .../install.ps1))) main RELAY KEY
#       the main machine, without a window: the line the relay printed
#   & ([scriptblock]::Create((irm .../install.ps1))) relay
#       the relay as a service (in a PowerShell run as administrator)
#
# The programs go to %LOCALAPPDATA%\Programs\BuildBalancer (BB_INSTALL_DIR names another folder),
# which is added to this user's PATH, and the app gets an entry in the start menu.  What is
# fetched is checked against the release's SHA256SUMS.  BB_RELEASE_URL names another place to
# fetch from (a mirror, a test); BB_NO_PATH=1 leaves PATH and the start menu alone.
param([string]$Action = '', [string]$First = '', [string]$Second = '')

$ErrorActionPreference = 'Stop'
# Windows PowerShell draws a progress bar for every block of a download, which makes a download
# of some megabytes take minutes
$ProgressPreference = 'SilentlyContinue'
$repo =if ($env:BB_REPO) { $env:BB_REPO } else { 'https://github.com/J4FF/Build-Balancer' }
$base = if ($env:BB_RELEASE_URL) { $env:BB_RELEASE_URL } else { "$repo/releases/latest/download" }
$dir = if ($env:BB_INSTALL_DIR) { $env:BB_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\BuildBalancer' }
# the programs for x86-64 run on ARM64 Windows too
$name = 'buildbalancer-windows-x86_64.zip'

# Windows PowerShell 5.1 does not offer TLS 1.2 by itself on older systems
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("bb-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $tmp | Out-Null
try {
    Write-Host "Build Balancer: fetching the programs for Windows ..."
    $zip = Join-Path $tmp $name
    $sums = Join-Path $tmp 'SHA256SUMS'
    try { Invoke-WebRequest "$base/$name" -OutFile $zip -UseBasicParsing }
    catch { throw "$base/$name could not be fetched ($($_.Exception.Message)). There may be no release yet: see $repo/releases, or build from the source (README, Building)." }
    Invoke-WebRequest "$base/SHA256SUMS" -OutFile $sums -UseBasicParsing

    $want = Get-Content $sums | ForEach-Object { $f = $_ -split '\s+'; if ($f.Count -ge 2 -and $f[1].TrimStart('*') -eq $name) { $f[0] } } | Select-Object -First 1
    $got = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    if (-not $want) { throw "SHA256SUMS does not list $name" }
    if ($want.ToLower() -ne $got) { throw "$name does not match its checksum (expected $want, got $got): nothing was installed" }

    $unpacked = Join-Path $tmp 'programs'
    Expand-Archive $zip -DestinationPath $unpacked -Force
    New-Item -ItemType Directory -Force $dir | Out-Null
    try { Copy-Item (Join-Path $unpacked '*') $dir -Recurse -Force }
    catch { throw "The programs in $dir could not be replaced ($($_.Exception.Message)). Is Build Balancer running? Close the app, stop bb-worker, and run this again." }
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
Write-Host "Installed into ${dir}:  bb  bb-worker  bb-relay  BuildBalancer"

if (-not $env:BB_NO_PATH) {
    # on this user's PATH, so `bb` works in every new terminal (and in this one)
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($userPath -split ';') -contains $dir)) {
        [Environment]::SetEnvironmentVariable('Path', (($userPath.TrimEnd(';') + ';' + $dir).TrimStart(';')), 'User')
        Write-Host "Added $dir to your PATH (new terminals have it)."
    }
    if (-not (($env:Path -split ';') -contains $dir)) { $env:Path = $env:Path.TrimEnd(';') + ';' + $dir }
    # the app in the start menu
    try {
        $link = Join-Path ([Environment]::GetFolderPath('Programs')) 'Build Balancer.lnk'
        $s = (New-Object -ComObject WScript.Shell).CreateShortcut($link)
        $s.TargetPath = Join-Path $dir 'BuildBalancer.exe'
        $s.WorkingDirectory = $dir
        $s.Description = 'Build Balancer: spread builds over the machines you own'
        $s.Save()
    }
    catch { Write-Host "(No start menu entry was made: $($_.Exception.Message))" }
}

switch ($Action) {
    '' {
        Write-Host ""
        Write-Host "Next: start `"Build Balancer`" from the start menu and choose what this machine is --"
        Write-Host "or in a terminal:  bb setup RELAY KEY   (main machine)   /   bb-worker join RELAY CODE   (lends power)"
    }
    'worker' {
        if (-not $First -or -not $Second) { throw "worker needs the relay's address and the code: both are shown by 'bb pair' (or `"Add a machine`") on the main machine" }
        & (Join-Path $dir 'bb-worker.exe') join $First $Second
    }
    'main' {
        if (-not $First -or -not $Second) { throw "main needs the relay's address and its setup key: the line the relay printed" }
        & (Join-Path $dir 'bb.exe') setup $First $Second
    }
    'relay' {
        & (Join-Path $dir 'bb-relay.exe') service install
    }
    default { throw "unknown action `"$Action`" (relay, worker RELAY CODE, main RELAY KEY, or none)" }
}
