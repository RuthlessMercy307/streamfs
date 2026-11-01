# Write-back volume through the real WinFsp mount: copy data far larger than the write cache,
# sample local disk use during the copy, remount, verify hashes.
# usage: .\vol-test.ps1 [-Store local|gdrive] [-CacheMB 64] [-UploadMBps 40] [-BigMB 1536] [-VolFolder streamfs-volumes/test1]
param([string]$Store = "local", [int]$CacheMB = 64, [int]$UploadMBps = 40, [int]$BigMB = 1536, [string]$VolFolder = "streamfs-volumes/test1", [string]$Drive = "U:", [switch]$SkipBig)
$bin = "C:\dev\sfs-target\release"; $state = "C:\dev\vol-state-$Store"; $remote = "C:\dev\vol-remote"; $stats = "C:\dev\vol-stats-$Store.json"
function Start-Vol {
  Get-Process sfs-volmount -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep 2
  $a = @('--store',$Store,'--mount',$Drive,'--state-dir',$state,'--write-cache-mb',$CacheMB,'--for-secs','3600','--stats-out',$stats)
  if ($Store -eq 'local') { $a += @('--remote-dir',$remote,'--upload-mbps',$UploadMBps,'--latency-ms','20') } else { $a += @('--vol-folder',$VolFolder) }
  Start-Process -FilePath "$bin\sfs-volmount.exe" -ArgumentList $a -WindowStyle Hidden -RedirectStandardOutput "C:\dev\vol-$Store.out" -RedirectStandardError "C:\dev\vol-$Store.err"
  $i = 0; while (-not (Test-Path "$Drive\") -and $i -lt 100) { Start-Sleep -Milliseconds 200; $i++ }
}
function Slab { $f = Get-Item "$state\wcache.slab" -ErrorAction SilentlyContinue; if ($f) { [math]::Round($f.Length/1MB) } else { 0 } }
Get-Process sfs-volmount -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep 1
Remove-Item $state -Recurse -Force -ErrorAction SilentlyContinue; if ($Store -eq 'local') { Remove-Item $remote -Recurse -Force -ErrorAction SilentlyContinue }
$src = "C:\dev\big-random-$BigMB.bin"
if (-not $SkipBig -and -not (Test-Path $src)) {
  $rng = [Security.Cryptography.RandomNumberGenerator]::Create(); $buf = New-Object byte[] (8MB); $fs = [IO.File]::Create($src)
  for ($k = 0; $k -lt $BigMB / 8; $k++) { $rng.GetBytes($buf); $fs.Write($buf, 0, $buf.Length) }; $fs.Close()
}
Start-Vol
$vi = Get-Volume -DriveLetter $Drive.TrimEnd(':') -ErrorAction SilentlyContinue
"volumen ${Drive}: tamano $([math]::Round($vi.Size/1GB,1)) GB, libre $([math]::Round($vi.SizeRemaining/1GB,1)) GB (cuota remota), cache de escritura $CacheMB MB"
$maxSlab = 0; $maxDirty = 0
if (-not $SkipBig) {
  $job = Start-Job { param($s, $d) Copy-Item $s "$d\big.bin" } -ArgumentList $src, $Drive
  $t0 = Get-Date
  while ($job.State -eq 'Running') { Start-Sleep -Milliseconds 500; $maxSlab = [math]::Max($maxSlab, (Slab)); try { $v = (Get-Content $stats -Raw | ConvertFrom-Json).volume; $maxDirty = [math]::Max($maxDirty, [math]::Round($v.dirty_bytes/1MB)) } catch {} }
  Receive-Job $job; $secs = ((Get-Date) - $t0).TotalSeconds
  "copia de $BigMB MB a ${Drive}: $([math]::Round($secs,1)) s ($([math]::Round($BigMB/$secs,1)) MB/s); slab local maximo $maxSlab MB; sucio maximo $maxDirty MB"
}
robocopy "C:\dev\ror-backup-20261030\Risk of Rain" "$Drive\Risk of Rain" /E /R:1 /W:1 /NFL /NDL /NJH /NJS | Out-Null
"robocopy Risk of Rain -> ${Drive}: exit $LASTEXITCODE"
"esperando a que todo se suba..."; $i = 0
do { Start-Sleep 2; $v = (Get-Content $stats -Raw | ConvertFrom-Json).volume; $i++ } while ($v.dirty_bytes -gt 0 -and $i -lt 900)
"pendiente de subir: $([math]::Round($v.dirty_bytes/1MB)) MB; subidos $([math]::Round($v.uploaded_bytes/1MB)) MB en $($v.upload_objects) objetos; esperas por contrapresion $($v.backpressure_waits) ($($v.backpressure_ms_total) ms); slab local $(Slab) MB; amplificacion RMW $($v.rmw_fetches)"
"== remontar con la cache de lectura vacia y verificar hashes"
Get-Process sfs-volmount | Stop-Process -Force; Start-Sleep 2; Remove-Item "$state\rcache" -Recurse -Force -ErrorAction SilentlyContinue
Start-Vol
if (-not $SkipBig) { $h1 = (Get-FileHash $src -Algorithm SHA256).Hash; $h2 = (Get-FileHash "$Drive\big.bin" -Algorithm SHA256).Hash; "big.bin: " + $(if ($h1 -eq $h2) { "SHA256 IGUAL" } else { "SHA256 DISTINTO $h1 vs $h2" }) }
$exp = Get-Content C:\dev\ror-backup-20261030\original-hashes.json -Raw | ConvertFrom-Json; $bad = 0
foreach ($e in $exp) { if ((Get-FileHash -LiteralPath "$Drive\Risk of Rain\$($e.Rel)" -Algorithm SHA256).Hash -ne $e.SHA256) { $bad++ } }
"Risk of Rain en ${Drive}: $($exp.Count) archivos, diferencias $bad"
$s = Get-Content $stats -Raw | ConvertFrom-Json; "lectura remota tras remontar: $([math]::Round($s.read_engine.backend_bytes/1MB)) MB en $($s.read_engine.backend_requests) peticiones"
if ($Store -eq 'local') { "remoto simulado: $((Get-ChildItem $remote).Count) objetos, $([math]::Round(((Get-ChildItem $remote | Measure-Object Length -Sum).Sum)/1MB)) MB" }
Get-Process sfs-volmount -ErrorAction SilentlyContinue | Stop-Process -Force
"errores: "; Get-Content "C:\dev\vol-$Store.err" -ErrorAction SilentlyContinue | Select -Last 5
