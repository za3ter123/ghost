# Builds ghost.exe (Rust, one binary: thin client, and the resident server under `--daemon`). Prints BUILD OK.
$root = $PSScriptRoot
$exe = Join-Path $root 'ghost.exe'

# Static CRT and delay-loaded GUI/COM DLLs: the client path then loads only kernel32 and ntdll (about 1 ms per call).
$delay = 'user32', 'oleaut32', 'combase', 'ole32', 'dwmapi', 'winmm', 'advapi32', 'gdi32', 'api-ms-win-core-winrt-l1-1-0', 'shell32', 'shlwapi', 'ws2_32' | ForEach-Object { "-C link-arg=/DELAYLOAD:$_.dll" }
$env:RUSTFLAGS = ((@('-C target-feature=+crt-static') + $delay + '-C link-arg=delayimp.lib') -join ' ')
Push-Location $root
try { $log = & cargo build --release --locked 2>&1 | Out-String; $code = $LASTEXITCODE } finally { Pop-Location }
if ($code -ne 0) { Write-Host $log; Write-Host "BUILD FAILED (cargo exit $code)"; exit 1 }

# A running daemon locks ghost.exe, but a locked exe can still be renamed: move it aside, put the new one in
# place, then stop the old daemon. The new client treats ghost.old*.exe and ghostd.exe in this folder as ours.
$ErrorActionPreference = 'Stop'
Get-ChildItem $root -Filter 'ghost.old*.exe' | ForEach-Object { Remove-Item $_.FullName -ErrorAction SilentlyContinue }
if (Test-Path $exe) { Move-Item $exe (Join-Path $root ('ghost.old-' + [DateTime]::Now.ToString('yyyyMMddHHmmss') + '.exe')) }
Copy-Item (Join-Path $root 'target\release\ghost.exe') $exe
& $exe daemon-stop | Out-Null
Start-Sleep -Milliseconds 300
Get-ChildItem $root -Filter 'ghost.old*.exe' | ForEach-Object { Remove-Item $_.FullName -ErrorAction SilentlyContinue }
'BUILD OK'
