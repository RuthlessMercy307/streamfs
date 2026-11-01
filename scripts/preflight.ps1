# Preflight before a big install: capacity (file 2x the write cache) and crash recovery
# against the REAL remote. Copies a random file, kills the mount mid-upload, remounts,
# lets uploads resume, empties the read cache and verifies the hash from Drive.
param([int]$SizeMB = 1024, [int]$CacheMB = 512, [int]$Parallel = 2, [string]$Folder = "streamfs-volumes/preflight", [string]$Drive = "U:", [int]$KillAfterSec = 90)
$bin = "C:\dev\sfs-target\release"; $state = "C:\dev\vol-state-preflight"; $stats = "C:\dev\vol-stats-preflight.json"
function Start-Pre($tag) {
  Start-Process -FilePath "$bin\sfs-volmount.exe" -ArgumentList @('--store','gdrive','--vol-folder',$Folder,'--mount',$Drive,'--state-dir',$state,'--write-cache-mb',$CacheMB,'--read-cache-mb','2048','--upload-parallel',$Parallel,'--for-secs','7200','--stats-out',$stats) -WindowStyle Hidden -RedirectStandardOutput "C:\dev\pre-$tag.out" -RedirectStandardError "C:\dev\pre-$tag.err"
  $i = 0; while (-not (Test-Path "$Drive\") -and $i -lt 100) { Start-Sleep -Milliseconds 300; $i++ }
}
function Vol { (Get-Content $stats -Raw | ConvertFrom-Json).volume }
Get-Process sfs-volmount -ErrorAction SilentlyContinue | ? { $_.CommandLine -like "*preflight*" } | Stop-Process -Force
Remove-Item $state -Recurse -Force -ErrorAction SilentlyContinue
$src = "C:\dev\big-random-$SizeMB.bin"
if (-not (Test-Path $src)) { $rng = [Security.Cryptography.RandomNumberGenerator]::Create(); $buf = New-Object byte[] (8MB); $fs = [IO.File]::Create($src); for ($k = 0; $k -lt $SizeMB / 8; $k++) { $rng.GetBytes($buf); $fs.Write($buf, 0, $buf.Length) }; $fs.Close() }
$h0 = (Get-FileHash $src -Algorithm SHA256).Hash
Start-Pre "a"; $t0 = Get-Date
$job = Start-Job { param($s, $d) Copy-Item $s "$d\big.bin" } -ArgumentList $src, $Drive
$maxSlab = 0; while ($job.State -eq 'Running' -and ((Get-Date) - $t0).TotalSeconds -lt $KillAfterSec) { Start-Sleep 1; $maxSlab = [math]::Max($maxSlab, (Get-Item "$state\wcache.slab" -ErrorAction SilentlyContinue).Length / 1MB) }
$v = Vol; "antes del kill ($([math]::Round(((Get-Date)-$t0).TotalSeconds)) s): subidos $([math]::Round($v.uploaded_bytes/1MB)) MB en $($v.upload_objects) objetos, pendientes $([math]::Round($v.dirty_bytes/1MB)) MB, contrapresion $($v.backpressure_waits), slab max $([math]::Round($maxSlab)) MB"
Get-Process sfs-volmount | ? { $_.Id -ne $null } | % { $cl = (Get-CimInstance Win32_Process -Filter "ProcessId=$($_.Id)").CommandLine; if ($cl -like "*preflight*") { Stop-Process -Id $_.Id -Force } }
Stop-Job $job -ErrorAction SilentlyContinue; Remove-Job $job -Force -ErrorAction SilentlyContinue
"KILL del montaje a mitad de subida (la copia queda interrumpida, como un corte)"
Start-Sleep 2; Start-Pre "b"
$size = (Get-Item "$Drive\big.bin" -ErrorAction SilentlyContinue).Length; "tras remontar: big.bin existe con $size bytes (copia interrumpida: se completa ahora)"
Copy-Item $src "$Drive\big.bin" -Force
$t1 = Get-Date; do { Start-Sleep 5; $v = Vol; $maxSlab = [math]::Max($maxSlab, (Get-Item "$state\wcache.slab").Length / 1MB) } while ($v.dirty_bytes -gt 0 -and ((Get-Date) - $t1).TotalMinutes -lt 60)
$up = ((Get-Date) - $t0).TotalSeconds
"subida completa: $([math]::Round($v.uploaded_bytes/1MB)) MB en esta sesion, $($v.upload_objects) objetos, errores $($v.upload_errors), contrapresion $($v.backpressure_waits) ($([math]::Round($v.backpressure_ms_total/1000)) s), slab max $([math]::Round($maxSlab)) MB, tiempo total $([math]::Round($up)) s"
Get-Process sfs-volmount | % { $cl = (Get-CimInstance Win32_Process -Filter "ProcessId=$($_.Id)").CommandLine; if ($cl -like "*preflight*") { Stop-Process -Id $_.Id -Force } }
Start-Sleep 2; Remove-Item "$state\rcache" -Recurse -Force; Start-Pre "c"
$t2 = Get-Date; $h1 = (Get-FileHash "$Drive\big.bin" -Algorithm SHA256).Hash; $rd = ((Get-Date) - $t2).TotalSeconds
"verificacion en frio desde Drive: " + $(if ($h0 -eq $h1) { "SHA256 IGUAL" } else { "SHA256 DISTINTO" }) + " ($SizeMB MB en $([math]::Round($rd)) s = $([math]::Round($SizeMB/$rd,1)) MB/s)"
"subidas registradas (velocidad por objeto):"; Get-Content C:\dev\pre-a.err, C:\dev\pre-b.err | Select-String "^\[upload\]" | Select -Last 4
Get-Process sfs-volmount | % { $cl = (Get-CimInstance Win32_Process -Filter "ProcessId=$($_.Id)").CommandLine; if ($cl -like "*preflight*") { Stop-Process -Id $_.Id -Force } }
