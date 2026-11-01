# Reproduce the long-stall incident: full sequential read (Get-FileHash) of a ~300 MB file
# through the mount. Compares engine configurations. Reports max read latency, blocked time
# and the slow-read / slow-fetch log lines emitted by the engine.
# usage: .\stall.ps1 -Label old -Extra '--no-promote --ra-init-kb 1024 --spec-conns 4' [-Backend sim|gdrive] [-File S:\seq.bin]
param([string]$Label, [string]$Extra = "", [string]$Backend = "sim", [string]$File = "S:\seq.bin", [string]$Drive = "S:")
$bin = "C:\dev\sfs-target\release"; $cache = "C:\dev\stall-cache"; $stats = "C:\dev\stall-stats-$Label.json"; $err = "C:\dev\stall-$Label.err"
Get-Process sfs-mount -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep 2
Remove-Item $cache -Recurse -Force -ErrorAction SilentlyContinue
$a = @('--backend',$Backend,'--mount',$Drive,'--cache-dir',$cache,'--cache-mb','2048','--for-secs','900','--stats-out',$stats,'--read-only') + ($Extra -split ' ' | ? { $_ })
if ($Backend -eq 'sim') { $a += @('--latency-ms','1000','--bw-mbps','8','--sim-seq-mb','300') }
Start-Process -FilePath "$bin\sfs-mount.exe" -ArgumentList $a -WindowStyle Hidden -RedirectStandardError $err
$i = 0; while (-not (Test-Path $File) -and $i -lt 100) { Start-Sleep -Milliseconds 200; $i++ }
$t = Measure-Command { $h = (Get-FileHash -Algorithm MD5 -LiteralPath $File).Hash }
Start-Sleep 3
$e = (Get-Content $stats -Raw | ConvertFrom-Json).engine
"[$Label] hash=$h  total=$([math]::Round($t.TotalSeconds,1)) s  MB/s=$([math]::Round((Get-Item $File).Length/1MB/$t.TotalSeconds,1))"
"[$Label] reads=$($e.reads) p99=$([math]::Round($e.lat_p99_us/1000)) ms  max_read=$([math]::Round($e.lat_max_us/1000)) ms  blocked_reads=$($e.blocked_reads) blocked_total=$($e.blocked_ms_total) ms  req=$($e.backend_requests) spec=$($e.speculative_requests) promoted=$($e.promoted_joins) req_max=$([math]::Round($e.backend_req_us_max/1000)) ms"
"[$Label] slow log lines: " + (Get-Content $err | Select-String "slow-").Count
Get-Content $err | Select-String "slow-" | Select -First 8
Get-Process sfs-mount -ErrorAction SilentlyContinue | Stop-Process -Force
