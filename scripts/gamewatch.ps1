# Records how a game process runs: image path, loaded modules from each drive, working
# set, start/exit times. Proves whether it really runs from the virtual drive.
# usage: .\gamewatch.ps1 -Name "Risk of Rain" -Out C:\dev\gamewatch.log [-Minutes 240]
param([string]$Name = "Risk of Rain", [string]$Out = "C:\dev\gamewatch.log", [int]$Minutes = 240)
$end = (Get-Date).AddMinutes($Minutes); $seen = @{}
while ((Get-Date) -lt $end) {
  foreach ($p in Get-Process -Name $Name -ErrorAction SilentlyContinue) {
    if (-not $seen.ContainsKey($p.Id)) {
      $seen[$p.Id] = $true
      Start-Sleep 3
      $mods = try { $p.Modules | % FileName } catch { @("(modules not readable: $($_.Exception.Message))") }
      $byDrive = $mods | Group-Object { if ($_ -match '^([A-Za-z]):') { $Matches[1].ToUpper() } else { '?' } } | % { "$($_.Name):$($_.Count)" }
      $parent = (Get-CimInstance Win32_Process -Filter "ProcessId=$($p.Id)").ParentProcessId
      $pp = (Get-CimInstance Win32_Process -Filter "ProcessId=$parent" -ErrorAction SilentlyContinue)
      $env = ""
      @("[$(Get-Date -Format 'HH:mm:ss')] START pid=$($p.Id) path=$($p.Path) start=$($p.StartTime.ToString('HH:mm:ss.fff'))",
        "  parent: $($pp.Name) ($($pp.ExecutablePath))",
        "  modules by drive: $($byDrive -join ' ')",
        "  game-dir modules: " + (($mods | ? { $_ -like '*Risk of Rain*' }) -join '; ')) | Add-Content $Out
    }
  }
  foreach ($id in @($seen.Keys)) {
    if ($seen[$id] -and -not (Get-Process -Id $id -ErrorAction SilentlyContinue)) { "[$(Get-Date -Format 'HH:mm:ss')] EXIT pid=$id" | Add-Content $Out; $seen[$id] = $false }
  }
  Start-Sleep 1
}
