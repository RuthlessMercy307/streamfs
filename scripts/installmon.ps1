# Big-install monitor: everything that can consume the local SSD while Steam installs
# into the StreamFS volume, plus the volume's own counters. Writes a STOP file and an
# alert line when C: free space drops below -AlertGB (the volume itself pauses writes at
# its own reserve). Locale-independent CSV.
# usage: .\installmon.ps1 -Stats C:\dev\vol-stats-steam1.json -State C:\dev\vol-state-steam1 -Out C:\dev\installmon.csv
param([string]$Stats, [string]$State, [string]$Out, [int]$IntervalSec = 15, [int]$Minutes = 1440, [int]$AlertGB = 20, [string]$AppId = "379430")
$ic = [Globalization.CultureInfo]::InvariantCulture
$steam = 'C:\Program Files (x86)\Steam'
function DirMB($p) { if (Test-Path $p) { ((Get-ChildItem $p -Recurse -File -Force -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum) / 1MB } else { 0 } }
$hdr = "time,c_free_GB,slab_MB,rcache_MB,journal_MB,logs_MB,steam_dl_C_MB,steam_temp_C_MB,depotcache_MB,appcache_MB,user_temp_MB,gdrivefs_MB,dirty_MB,uploaded_MB,objects,bp_waits,reserve_waits,zero_blocks,logical_MB,manifest_state,bytes_downloaded_MB,bytes_to_download_MB,bytes_staged_MB,drive_upload_req,drive_media_req,drive_403,drive_429,upload_errors"
$hdr | Set-Content $Out
$end = (Get-Date).AddMinutes($Minutes); $alerted = $false
while ((Get-Date) -lt $end) {
  try {
    $j = Get-Content $Stats -Raw | ConvertFrom-Json; $v = $j.volume; $d = $j.drive
    $cfree = ([IO.DriveInfo]::new('C')).AvailableFreeSpace / 1GB
    $slab = (Get-Item "$State\wcache.slab" -ErrorAction SilentlyContinue).Length / 1MB
    $rc = DirMB "$State\rcache"
    $jr = (Get-Item "$State\volume.jsonl" -ErrorAction SilentlyContinue).Length / 1MB
    $logs = ((Get-ChildItem 'C:\dev\vol-steam1*.err','C:\dev\vol-steam1*.out' -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum) / 1MB
    $acf = "S:\SteamLibrary\steamapps\appmanifest_$AppId.acf"; $m = if (Test-Path $acf) { Get-Content $acf -Raw } else { '' }
    $g = { param($k) if ($m -match ('"' + $k + '"\s+"(\d+)"')) { [double]$Matches[1] } else { 0 } }
    $line = [string]::Format($ic, "{0},{1:F2},{2:F0},{3:F0},{4:F1},{5:F1},{6:F0},{7:F0},{8:F0},{9:F0},{10:F0},{11:F0},{12:F0},{13:F0},{14},{15},{16},{17},{18:F0},{19},{20:F0},{21:F0},{22:F0},{23},{24},{25},{26},{27}",
      (Get-Date -Format 'HH:mm:ss'), $cfree, $slab, $rc, $jr, $logs,
      (DirMB "$steam\steamapps\downloading"), (DirMB "$steam\steamapps\temp"), (DirMB "$steam\depotcache"), (DirMB "$steam\appcache"),
      (DirMB $env:TEMP), (DirMB "$env:LOCALAPPDATA\Google\DriveFS"),
      $v.dirty_bytes/1MB, $v.uploaded_bytes/1MB, $v.upload_objects, $v.backpressure_waits, $v.reserve_waits, $v.zero_blocks_elided, $v.logical_bytes/1MB,
      (& $g 'StateFlags'), (& $g 'BytesDownloaded')/1MB, (& $g 'BytesToDownload')/1MB, (& $g 'BytesStaged')/1MB,
      $d.upload_requests, $d.media_requests, $d.http_403, $d.http_429, $v.upload_errors)
    $line | Add-Content $Out
    if ($cfree -lt $AlertGB -and -not $alerted) {
      "[$(Get-Date -Format 'HH:mm:ss')] ALERTA: C: con $([math]::Round($cfree,1)) GB libres (< $AlertGB GB). Revisar consumo; el volumen pausa escrituras a su propia reserva." | Add-Content "$Out.alerts"
      $alerted = $true
    }
  } catch {}
  Start-Sleep $IntervalSec
}
