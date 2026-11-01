# Samples the write-back volume during a real Steam install: local SSD use, pending/
# uploaded bytes, objects, backpressure, Drive requests/errors. Locale-independent CSV.
# usage: .\volmon.ps1 -Stats C:\dev\vol-stats-steam1.json -State C:\dev\vol-state-steam1 -Out C:\dev\volmon-steam1.csv
param([string]$Stats, [string]$State, [string]$Out, [int]$IntervalSec = 5, [int]$Minutes = 600)
$ic = [Globalization.CultureInfo]::InvariantCulture
"time,c_free_GB,slab_MB,rcache_MB,dirty_MB,dirty_peak_MB,uploaded_MB,objects,bp_waits,bp_s,rmw,logical_MB,meta_snaps,drive_upload_req,drive_media_req,drive_403,drive_429" | Set-Content $Out
$end = (Get-Date).AddMinutes($Minutes)
while ((Get-Date) -lt $end) {
  try {
    $j = Get-Content $Stats -Raw | ConvertFrom-Json; $v = $j.volume; $d = $j.drive
    $slab = (Get-Item "$State\wcache.slab" -ErrorAction SilentlyContinue).Length / 1MB
    $rc = ((Get-ChildItem "$State\rcache" -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum) / 1MB
    $cfree = ([IO.DriveInfo]::new('C')).AvailableFreeSpace / 1GB
    [string]::Format($ic, "{0},{1:F2},{2:F0},{3:F0},{4:F0},{5:F0},{6:F0},{7},{8},{9:F0},{10},{11:F0},{12},{13},{14},{15},{16}",
      (Get-Date -Format 'HH:mm:ss'), $cfree, $slab, $rc, $v.dirty_bytes/1MB, $v.dirty_peak_bytes/1MB, $v.uploaded_bytes/1MB, $v.upload_objects,
      $v.backpressure_waits, $v.backpressure_ms_total/1000, $v.rmw_fetches, $v.logical_bytes/1MB, $v.meta_snapshots,
      $d.upload_requests, $d.media_requests, $d.http_403, $d.http_429) | Add-Content $Out
  } catch {}
  Start-Sleep $IntervalSec
}
