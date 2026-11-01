# Keeps N independent SSH tunnels (one TCP connection each) to sfs-server alive.
# Local ports $Ports -> VPS 127.0.0.1:$RemotePort. A tunnel that exits is restarted with
# exponential backoff (3 s .. 60 s, reset after 120 s up). Starts are staggered so reconnects
# stay under the VPS ufw limit (6 new SSH connections / 30 s).
# uso: tunnel.ps1 -HostName <ip-o-dominio-del-VPS> [-User sfs] [-Ports 8787,8788,8789,8790] [-Minutes 0 (= forever)]
param(
    [int[]]$Ports = @(8787, 8788, 8789, 8790),
    [int]$RemotePort = 8787,
    [Parameter(Mandatory)][string]$HostName,
    [string]$User = 'sfs',
    [string]$Key = "$env:USERPROFILE\.ssh\streamfs_ed25519",
    [string]$Log = "$env:TEMP\streamfs-tunnel.log",
    [int]$Minutes = 0
)
$ssh = 'C:\Windows\System32\OpenSSH\ssh.exe'
function Note($m) { "$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') $m" | Add-Content -Path $Log }

$slots = foreach ($p in $Ports) { [pscustomobject]@{ Port = $p; Proc = $null; Backoff = 3; NextStart = [datetime]::MinValue; Started = $null; Restarts = 0 } }
$lastStart = [datetime]::MinValue
$end = if ($Minutes -gt 0) { (Get-Date).AddMinutes($Minutes) } else { [datetime]::MaxValue }
Note "supervisor up: ports $($Ports -join ',') -> ${HostName}:$RemotePort"
try {
    while ((Get-Date) -lt $end) {
        foreach ($s in $slots) {
            if ($s.Proc -and -not $s.Proc.HasExited) {
                if ($s.Started -and ((Get-Date) - $s.Started).TotalSeconds -gt 120) { $s.Backoff = 3 }
                continue
            }
            if ($s.Proc) {
                Note "port $($s.Port) exited (code $($s.Proc.ExitCode)); retry in $($s.Backoff)s"
                $s.NextStart = (Get-Date).AddSeconds($s.Backoff)
                $s.Backoff = [math]::Min($s.Backoff * 2, 60)
                $s.Proc = $null
                $s.Restarts++
            }
            # stagger: at most one new connection every 6 s
            if ((Get-Date) -lt $s.NextStart -or ((Get-Date) - $lastStart).TotalSeconds -lt 6) { continue }
            $sargs = @('-i', $Key, '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes', '-o', 'ExitOnForwardFailure=yes',
                '-o', 'ServerAliveInterval=10', '-o', 'ServerAliveCountMax=3', '-o', 'ConnectTimeout=15', '-o', 'TCPKeepAlive=yes',
                '-N', '-L', "127.0.0.1:$($s.Port):127.0.0.1:$RemotePort", "$User@$HostName")
            $s.Proc = Start-Process -FilePath $ssh -ArgumentList $sargs -WindowStyle Hidden -PassThru -RedirectStandardError "$env:TEMP\streamfs-tunnel-$($s.Port).err"
            $s.Started = Get-Date
            $lastStart = Get-Date
            Note "port $($s.Port) started pid $($s.Proc.Id) (restarts $($s.Restarts))"
        }
        Start-Sleep -Seconds 2
    }
} finally {
    foreach ($s in $slots) { if ($s.Proc -and -not $s.Proc.HasExited) { Stop-Process -Id $s.Proc.Id -Force -ErrorAction SilentlyContinue } }
    Note 'supervisor down'
}
