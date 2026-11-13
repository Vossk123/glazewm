# Deploys the stage-manager build of glazewm.exe + CLI over the official install.
# Must run elevated. Backs up the official exes once (never overwrites a backup).
$ErrorActionPreference = 'Stop'
$src = 'C:\Code\glazewm-stage-manager\target\release\glazewm.exe'
$dst = 'C:\Program Files\glzr.io\GlazeWM\glazewm.exe'
$srcCli = 'C:\Code\glazewm-stage-manager\target\release\glazewm-cli.exe'
$dstCli = 'C:\Program Files\glzr.io\GlazeWM\cli\glazewm.exe'

# Stop running instances (test build and/or official).
Get-Process glazewm* -ErrorAction SilentlyContinue | ForEach-Object { $_.Kill(); $_.WaitForExit(5000) }
Start-Sleep -Milliseconds 800

if (-not (Test-Path "$dst.stage-backup")) {
  Copy-Item $dst "$dst.stage-backup"
}
Copy-Item $src $dst -Force

# Fork CLI adds the toggle-stage-manager subcommand the official CLI lacks.
if (-not (Test-Path "$dstCli.stage-backup")) {
  Copy-Item $dstCli "$dstCli.stage-backup"
}
Copy-Item $srcCli $dstCli -Force

# Restart on the interactive desktop (autostart is the HKCU Run key; starting
# from here launches the same exe the Run key will use next boot).
Start-Process -FilePath $dst

Write-Output "DEPLOY_OK"
