# Two instances of the app on one computer: A (original) and B (a copy of A's folder = a second device of
# the same account). For checking multi-device behaviour through the real shell without a second machine
# (docs/decisions/multi-device-equal-nodes.md). Needs a Debug build of the shell
# (dotnet build windows-app/Omni.Windows.sln -p:Platform=x64) and kills only THAT exe, so a stand app
# running from another folder is left alone.
#
# usage: two-instance-local.ps1 -Step start   # fresh scratch dir, profile created on A
#        two-instance-local.ps1 -Step copy    # stop A, copy its folder to B, start both
#        two-instance-local.ps1 -Step cmd -Node A|B -Cmd 'mesh.start'   # test-channel command
#        two-instance-local.ps1 -Step kill
# Run 'dotnet build' only after -Step kill: running instances lock omni_ffi.dll and the copy fails.
param(
    [string]$Step = 'start',
    [string]$Node = 'A',
    [string]$Cmd = ''
)
$root = Join-Path $env:TEMP 'omni-two-instance'
$exe = Join-Path (Split-Path -Parent $PSScriptRoot) 'windows-app\src\Omni.Windows.App\bin\x64\Debug\net10.0-windows10.0.19041.0\win-x64\Omni.Windows.App.exe'

function Start-Node([string]$name) {
    $data = Join-Path $root "data$name"
    $chan = Join-Path $root "ch$name"
    New-Item -ItemType Directory -Force $chan | Out-Null
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.WorkingDirectory = Split-Path $exe
    $psi.UseShellExecute = $false
    $psi.EnvironmentVariables['OMNI_APPDATA_DIR'] = $data
    $psi.EnvironmentVariables['OMNI_UI_TEST_CHANNEL'] = $chan
    $psi.EnvironmentVariables['OMNI_TRACE_FILE'] = (Join-Path $root "trace$name.log")
    [System.Diagnostics.Process]::Start($psi) | Out-Null
}

function Send-Cmd([string]$name, [string]$text) {
    $chan = Join-Path $root "ch$name"
    $id = 'c' + [guid]::NewGuid().ToString('N').Substring(0, 8)
    $tmp = Join-Path $chan "$id.tmp"
    [System.IO.File]::WriteAllText($tmp, $text, (New-Object System.Text.UTF8Encoding($false)))
    Move-Item $tmp (Join-Path $chan "$id.cmd")
    $out = Join-Path $chan "$id.out"
    for ($i = 0; $i -lt 120; $i++) {
        if (Test-Path $out) { Start-Sleep -Milliseconds 300; Get-Content $out -Raw -Encoding UTF8; Remove-Item $out -Force; return }
        Start-Sleep -Milliseconds 500
    }
    'TIMEOUT'
}

switch ($Step) {
    'kill' {
        Get-Process -Name 'Omni.Windows.App' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force
    }
    'start' {
        if (Test-Path $root) { Remove-Item $root -Recurse -Force }
        New-Item -ItemType Directory -Force $root | Out-Null
        Start-Node 'A'
        Start-Sleep 12
        Send-Cmd 'A' 'profile.create Alice AlicePC'
        Start-Sleep 3
        'A created'
    }
    'copy' {
        # A's folder -> B's folder, with A stopped, then both back up.
        Get-Process -Name 'Omni.Windows.App' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force
        Start-Sleep 3
        Copy-Item (Join-Path $root 'dataA') (Join-Path $root 'dataB') -Recurse -Force
        Start-Node 'A'
        Start-Node 'B'
        Start-Sleep 15
        'A and B up'
    }
    'cmd' { Send-Cmd $Node $Cmd }
}
