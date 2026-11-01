# Network/disk congestion logger (locale-independent: no perf-counter names).
# Every $IntervalSec: total NIC bytes in/out of the PC, and I/O bytes of steam.exe,
# sfs-mount.exe and the game, so a slow read can be matched against competing traffic.
# usage: .\netmon.ps1 -Out C:\dev\netmon.csv [-IntervalSec 2] [-Minutes 240]
param([string]$Out = "C:\dev\netmon.csv", [int]$IntervalSec = 2, [int]$Minutes = 240)
function NicTotals {
  $rx = 0; $tx = 0
  foreach ($n in [System.Net.NetworkInformation.NetworkInterface]::GetAllNetworkInterfaces()) {
    if ($n.OperationalStatus -ne 'Up' -or $n.NetworkInterfaceType -eq 'Loopback') { continue }
    $s = $n.GetIPStatistics(); $rx += $s.BytesReceived; $tx += $s.BytesSent
  }
  @($rx, $tx)
}
function ProcIo($names) {
  $h = @{}
  foreach ($p in Get-CimInstance Win32_Process -Filter "Name='steam.exe' OR Name='steamwebhelper.exe' OR Name='sfs-mount.exe' OR Name='Risk of Rain.exe'") {
    $k = $p.Name; if (-not $h.ContainsKey($k)) { $h[$k] = @(0, 0) }
    $h[$k] = @($h[$k][0] + $p.ReadTransferCount, $h[$k][1] + $p.WriteTransferCount)
  }
  $h
}
"time,nic_rx_MBps,nic_tx_MBps,steam_read_MBps,steam_write_MBps,sfs_read_MBps,sfs_write_MBps,game_read_MBps" | Set-Content $Out
$prevNic = NicTotals; $prevIo = ProcIo; $end = (Get-Date).AddMinutes($Minutes)
while ((Get-Date) -lt $end) {
  Start-Sleep $IntervalSec
  $nic = NicTotals; $io = ProcIo
  $d = { param($k, $i) if ($io.ContainsKey($k) -and $prevIo.ContainsKey($k)) { [math]::Max(0, ($io[$k][$i] - $prevIo[$k][$i])) / 1MB / $IntervalSec } else { 0 } }
  $steamR = (& $d 'steam.exe' 0) + (& $d 'steamwebhelper.exe' 0); $steamW = (& $d 'steam.exe' 1) + (& $d 'steamwebhelper.exe' 1)
  [string]::Format([Globalization.CultureInfo]::InvariantCulture, "{0},{1:F2},{2:F2},{3:F2},{4:F2},{5:F2},{6:F2},{7:F2}", (Get-Date -Format 'HH:mm:ss'), (($nic[0]-$prevNic[0])/1MB/$IntervalSec), (($nic[1]-$prevNic[1])/1MB/$IntervalSec), $steamR, $steamW, (& $d 'sfs-mount.exe' 0), (& $d 'sfs-mount.exe' 1), (& $d "Risk of Rain.exe" 0)) | Add-Content $Out
  $prevNic = $nic; $prevIo = $io
}
