# Boot experiment through the real WinFsp mount (simulated drive-like remote or Drive).
# Conditions: A) empty cache  B) empty cache + startup profile prefetch  C) prepared cache
# usage: .\boot.ps1 -Backend sim|gdrive [-File W:\game.pak] [-Reads 20] [-HeadStartSec 0]
param([string]$Backend = "sim", [string]$File = "W:\game.pak", [string]$Mount = "W:", [int]$Reads = 20, [int]$HeadStartSec = 0, [string]$Extra = "", [int]$LatencyMs = 1000, [int]$Bw = 8)
$bin = "C:\dev\sfs-target\release"; $cache = "C:\dev\boot-cache"; $prof = "C:\dev\boot-profile-$Backend.json"
function Start-Mount($extra) {
  Get-Process sfs-mount -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep 2
  $a = @('--backend',$Backend,'--mount',$Mount,'--cache-dir',$cache,'--cache-mb','2048','--for-secs','400') + ($extra -split ' ' | ? { $_ })
  if ($Backend -eq 'sim') { $a += @('--latency-ms',$LatencyMs,'--bw-mbps',$Bw) }
  Start-Process -FilePath "$bin\sfs-mount.exe" -ArgumentList $a -WindowStyle Hidden
  $i = 0; while (-not (Test-Path $File) -and $i -lt 100) { Start-Sleep -Milliseconds 200; $i++ }
}
function Run-Boot($label) {
  if ($HeadStartSec -gt 0) { Start-Sleep $HeadStartSec }
  & "$bin\mount-bench.exe" --file $File --reads $Reads --size 1048576 --seed 9 --boot --label $label --out "C:\dev\boot-$label.json" | Select-String "BOOT total|boot reads"
}
Remove-Item $cache -Recurse -Force -ErrorAction SilentlyContinue; Remove-Item $prof -ErrorAction SilentlyContinue
"== A) cache vacia (grabando perfil)"
Start-Mount "--profile-out $prof $Extra"; Run-Boot "A-vacia"; Start-Sleep 3
"== B) cache vacia + perfil de arranque (precarga al montar, ventaja inicial ${HeadStartSec}s)"
Get-Process sfs-mount | Stop-Process -Force; Start-Sleep 2; Remove-Item $cache -Recurse -Force -ErrorAction SilentlyContinue
Start-Mount "--profile-in $prof $Extra"; Run-Boot "B-perfil"
"== C) cache preparada (tras A+B, sin perfil)"
Start-Mount "$Extra"; Run-Boot "C-preparada"
Get-Process sfs-mount -ErrorAction SilentlyContinue | Stop-Process -Force
