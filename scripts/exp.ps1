# Reproducible mount experiment against the SIMULATED drive-like profile
# (calibrated to real Drive: ~1.0 s latency, ~8 MB/s per connection => ~13 MB/s at 8 conns).
# usage: .\exp.ps1 -Label base -Extra '--min-fetch-kb 1024' [-Reads 20] [-SeqMb 48]
param([string]$Label, [string]$Extra = "", [int]$Reads = 20, [int]$SeqMb = 48, [int]$LatencyMs = 1000, [int]$Bw = 8)
$bin = "C:\dev\sfs-target\release"
Get-Process sfs-mount -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep 2
$cache = "C:\dev\exp-cache"; Remove-Item $cache -Recurse -Force -ErrorAction SilentlyContinue
$stats = "C:\dev\exp-stats-$Label.json"; $out = "C:\dev\exp-bench-$Label.json"
$args = @('--backend','sim','--mount','W:','--cache-dir',$cache,'--cache-mb','2048','--latency-ms',$LatencyMs,'--bw-mbps',$Bw,'--for-secs','300','--stats-out',$stats) + ($Extra -split ' ' | ? { $_ })
Start-Process -FilePath "$bin\sfs-mount.exe" -ArgumentList $args -WindowStyle Hidden
$i = 0; while (-not (Test-Path W:\game.pak) -and $i -lt 50) { Start-Sleep -Milliseconds 200; $i++ }
& "$bin\mount-bench.exe" --file W:\game.pak --reads $Reads --size 1048576 --seq-mb $SeqMb --seed 3 --label $Label --out $out
Start-Sleep 3
$d = Get-Content $stats -Raw | ConvertFrom-Json; $e = $d.engine
"engine: backend_req=$($e.backend_requests) spec_req=$($e.speculative_requests) net_MB=$([math]::Round($e.backend_bytes/1MB,1)) spec_MB=$([math]::Round($e.speculative_bytes/1MB,1)) hit=$($e.blocks_hit) miss=$($e.blocks_missed) retries=$($e.retries) conn_limit=$($d.conn_limit)"
Get-Process sfs-mount -ErrorAction SilentlyContinue | Stop-Process -Force
