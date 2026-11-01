# Copy-on-write verification through the real WinFsp mount (sim backend).
# write -> remount same overlay (check) -> mount with fresh overlay (base untouched).
$bin = "C:\dev\sfs-target\release"; $ov = "C:\dev\cow-overlay"; $ov2 = "C:\dev\cow-overlay-fresh"
function Start-SfsMount($overlay, $cache) {
  Get-Process sfs-mount -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep 2
  Start-Process -FilePath "$bin\sfs-mount.exe" -ArgumentList @('--backend','sim','--mount','S:','--cache-dir',$cache,'--cache-mb','512','--overlay-dir',$overlay,'--for-secs','200','--stats-out','C:\dev\cow-stats.json') -WindowStyle Hidden -RedirectStandardError C:\dev\cow-mount.err
  $i = 0; while (-not (Test-Path S:\small.bin) -and $i -lt 50) { Start-Sleep -Milliseconds 200; $i++ }
}
Remove-Item $ov,$ov2,C:\dev\cow-cache1,C:\dev\cow-cache2,C:\dev\cow-cache3 -Recurse -Force -ErrorAction SilentlyContinue
"== fase write"; Start-SfsMount $ov 'C:\dev\cow-cache1'; & "$bin\verify-mount.exe" --root 'S:\' --cow write
Start-Sleep 3; "cow stats: " + ((Get-Content C:\dev\cow-stats.json -Raw | ConvertFrom-Json).cow | ConvertTo-Json -Compress)
"engine net MB: " + [math]::Round(((Get-Content C:\dev\cow-stats.json -Raw | ConvertFrom-Json).engine.backend_bytes)/1MB,1)
"== fase check (remontado tras kill, cache vacia, misma capa)"; Start-SfsMount $ov 'C:\dev\cow-cache2'; & "$bin\verify-mount.exe" --root 'S:\' --cow check
"== fase base (capa nueva vacia)"; Start-SfsMount $ov2 'C:\dev\cow-cache3'; & "$bin\verify-mount.exe" --root 'S:\' --cow base
"== verificacion general de lectura (capa vacia)"; & "$bin\verify-mount.exe" --root 'S:\' | Select-String "RESULTADO|FAIL"
Get-Process sfs-mount -ErrorAction SilentlyContinue | Stop-Process -Force
"== errores del montaje:"; Get-Content C:\dev\cow-mount.err -ErrorAction SilentlyContinue | Select -Last 5
