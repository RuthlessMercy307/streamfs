# Diario de desarrollo

Notas de trabajo tal como se escribieron durante el desarrollo (mediciones, decisiones, errores). Rutas como C:/dev/... son de la máquina de desarrollo.

# streamfs — estado real (2026-10-30)

## Qué existe y está verificado
| Pieza | Estado | Evidencia |
|---|---|---|
| `Backend` trait (stat/list/read_range, errores tipados) | hecho | `crates/core/src/backend.rs` |
| Almacén de bloques persistente (slab + journal JSONL, CRC32, LRU, pin, recuperación) | hecho | 12 tests |
| Motor de lecturas (singleflight, coalescing, min/max fetch, read-ahead adaptativo, reintentos con backoff, AIMD de concurrencia, prioridad crítica > especulativa) | hecho | 12 tests + benchmark |
| Backend simulado (archivos sintéticos verificables byte a byte, latencia, ancho de banda, 429/reset inyectados) | hecho | `crates/sim` |
| Benchmark reproducible | hecho (SOLO simulado) | `cargo run --release -p sfs-bench` → `bench-sim.json` |

Pruebas (`cargo test -p sfs-sim`): lecturas aleatorias correctas en archivo de 20 GB sin descargarlo; lectura de 10 MB de 20 GB baja ≤13 MB; dedupe de 16 lecturas concurrentes = 1 petición; caché persistente entre reinicios con 0 tráfico; expulsión respeta tope; corrupción de la caché detectada y recuperada (mutación de CRC comprobada: la prueba falla sin la verificación); invalidación por cambio de versión; reintentos 429/reset; journal truncado tras crash; archivo fijado nunca expulsado y desborde falla en vez de exceder el tope.

## Resultados del simulador (NO son de Drive)
Perfil: 120 ms + jitter, 12 MB/s por conexión, 40 MB/s total.
- Aleatorio frío p50: 256K 231 ms, 1M 250 ms, 4M 508 ms, 16M 1079 ms.
- Secuencial 256 MB: 20.1 MB/s con read-ahead vs 4.3 MB/s sin él (no satura los 40 MB/s: pendiente de ajustar).
- Caliente: p50 0.85 ms, 0 bytes de red.
- Con 1/4 peticiones en 429 y 1/7 reset: 0/100 lecturas fallidas, concurrencia baja a 1.

## NO hecho todavía (sin maquillar)
1. Backend Google Drive (HTTP Range + OAuth loopback/PKCE). Necesita client ID propio del usuario.
2. Montaje WinFsp. WinFsp NO está instalado; `winfsp-sys` probablemente necesita LLVM/libclang (no instalado).
3. Perfiles de arranque / precarga predictiva (necesita telemetría de trazas; el motor ya cuenta hits/misses/latencias).
4. Cola de escritura / capa copy-on-write, saves locales.
5. UI, integración Steam, overlay de espera.
6. Ninguna prueba con juegos reales, red real ni Drive real.

## Riesgos de la investigación (docs/research-drive-winfsp.md)
- Cuota por archivo de Drive ("too much traffic") sin umbral oficial documentado → bloques grandes, caché agresiva, no releer.
- `drive.readonly` es scope restringido: en modo Testing el refresh token caduca a 7 días; distribuir públicamente exige verificación.
- WinFsp y `winfsp-rs` son GPL-3.0: distribuir un producto propietario exige licencia comercial de WinFsp. Por eso el motor (`sfs-core`) no depende de WinFsp.
- Steam sobre sistema de archivos remoto: sin evidencia oficial; se probará, no se asume.

## Drive real (2026-10-30) — primera medicion, cuenta <cuenta de Google del autor> (limite 5 TiB, ~10 GB usados)
Archivo: .7z de 1.87 GB (el mas grande de la cuenta). Una sola ejecucion, muestras pequenas (n=6 por tamano), un solo momento del dia: orientativo, NO concluyente.
- Lecturas aleatorias directas, 1 conexion, p50: 256K 1115 ms, 1M 1239 ms, 4M 1396 ms, 16M 2602 ms (p95 16M: 7278 ms). Latencia fija ~1.1 s por peticion.
- Concurrencia (2 MB por peticion): 1 conn 3.0 MB/s, 2 conn 5.2, 4 conn 8.9, 8 conn 13.3. Sin 403 ni 429 en 56 peticiones. 8 conexiones no saturo: no se probo mas.
- Motor sobre Drive: 10 MB aleatorios en frio 2015 ms, en caliente 6 ms; bajo 10.7 MB de 1.87 GB (0.58%) => descarga parcial por rango funciona en Drive real.
- Cuota de la ejecucion: 1 peticion de metadatos + 56 de medios, 207.4 MB, 0 errores.
Conclusiones provisionales: el coste dominante es la latencia por peticion, no el ancho de banda (4 MB cuesta ~13% mas que 1 MB) => subir min_fetch a ~4 MB y paralelizar es casi gratis; un juego con lecturas pequeñas dispersas y sin precarga se sentira lento. Falta: probar >8 conexiones, otras horas, y un archivo >10 GB para ver el limite por archivo.

---
## Montaje WinFsp + optimizaciones (2026-10-30, tarde)

### Hecho y verificado
- WinFsp 2.1 (winget `WinFsp.WinFsp`) y LLVM 23 (winget `LLVM.LLVM`, necesario: `winfsp-sys` usa bindgen) instalados con tu autorizacion. Monta sin permisos de administrador.
- `sfs-mount`: unidad de solo lectura con lecturas asincronas (`AsyncFileSystemContext`): una lectura bloqueada aparca una tarea tokio, no un hilo del dispatcher de WinFsp.
- `verify-mount` (sim, 20 GB logicos) sobre la unidad real: tamaño, listado, 40 lecturas aleatorias sin diferencias, mmap de 8 MB y de una ventana de 16 MB a 5 GB, 4 lectores concurrentes, EOF, escritura rechazada. TODO OK.
- Ejecutar un .exe desde la unidad (backend local con 120 ms simulados): arranque en frio 1080 ms, en caliente 31 ms (n=1; el baseline local de 87 ms es ruido de primera ejecucion/antivirus, no se concluye nada de esa comparacion).
- Integridad real: MD5 del .zip de 285 MB leido por el montaje == MD5 de Drive (`2d1770566eafef446ac7730458c5a777`).
- Caché persistente tras `Stop-Process -Force` (cierre inesperado): reinicio con cache preparada => primera lectura 1.5 ms, 0 esperas >500 ms, 82.8 MB de slab.
- 15 pruebas automaticas (`cargo test -p sfs-sim`), con comprobacion por mutacion de las dos mas criticas (CRC y promocion de prioridad).

### Arranque (20 lecturas de 1 MB dispersas, a traves del montaje; tiempo hasta que el programa puede continuar)
```yaml
metric: boot_total_ms (20 scattered 1 MB reads)
environment: Windows 11, Ryzen 5 3600, WinFsp mount, 8 conexiones, cache 2 GB
real_drive (.zip 285 MB en tu Drive):
  A_cache_vacia: 21663 ms   (p50 956 ms por lectura, 20/20 esperas >500 ms)
  B_cache_vacia_con_perfil: 3429 ms  (6.3x; ventaja inicial 0 s)   # perfil grabado en la corrida A
  C_cache_preparada: 22 ms
simulated_drive_like (1.0 s, 8 MB/s por conexion):
  A: 23391 ms   B: 3373 ms   C: 24 ms
method: 1 corrida por condicion en Drive real (dos corridas A: 19307 y 21663 ms), 1 en simulador
```
Limites: una sola corrida por condicion; el perfil se grabo del mismo patron que luego se reproduce (el mejor caso: un juego real tiene variacion entre arranques); sin ventaja inicial. No es un juego real.

### Optimizaciones: que se probo y que no (simulador calibrado, 48 MB secuenciales)
| cambio | resultado | decision |
|---|---|---|
| base | 6.7 MB/s, p99 1285 ms | — |
| promocion de especulativa a critica cuando una lectura se une | 6.8 MB/s (sin efecto en este caso; corrige inversion de prioridad, probada con test determinista) | conservado |
| ventana inicial de read-ahead 8 MB | 9.6 MB/s (+41 %), espera maxima 2.0 s -> 1.1 s | defecto nuevo |
| tope de especulativas = todas las conexiones menos una (antes la mitad) | precarga de perfil 5.7 s -> 3.4 s | defecto nuevo |
| min_fetch 4 MB | lectura aleatoria p50 +30 % y 2x de red | RECHAZADO |
Drive real, mismo comando antes/despues (cache vacia, 30 aleatorias + 32 MB secuenciales): secuencial 5.7 -> 7.9 MB/s, esperas >500 ms 6 -> 4, p99 secuencial 1063 -> 939 ms. Una corrida: los extremos de latencia aleatoria variaron (p99 1455 -> 2032 ms) y no se atribuyen a ningun cambio.
Defecto encontrado por un test: el read-ahead de 8 MB expulsaba bloques recien leidos en cachés minusculas; ahora la ventana se limita a 1/8 de la cache.

### Problemas encontrados
1. Sin `system`, `winfsp-rs` necesita `winfsp-x64.dll` junto al .exe (error 1285 DELAY_LOAD_FAILED): copiarla desde `C:\Program Files (x86)\WinFsp\bin`. El instalador basico no trae headers: usar los vendorizados del crate.
2. La ruta larga del directorio del proyecto rompe el enlazado (LNK1104): `.cargo/config.toml` apunta `target-dir` a `C:/dev/sfs-target`.
3. Listado de directorios: se resuelve con `block_on` en un hilo del dispatcher y la cache de listados es solo en memoria (300 s). Reabrir tras remontar costo 1177 ms en Drive. Pendiente: persistir el arbol de directorios.
4. Una lectura quedo bloqueada 20.5 s durante la lectura completa del .zip (antes de promocion/limite de especulativas). Causa NO confirmada (hipotesis: inversion de prioridad o cola larga de Drive); no se reprodujo desde entonces, pero no se repitio la lectura completa. Estado: sin verificar.
5. Los 5 TiB de la cuenta son el limite real (confirmado por el usuario); el archivo grande de 1.87 GB esta fuera de "Mi unidad" (sin carpeta padre).

### Sigue sin hacerse / sin probar
- Ningun juego real ni Steam. Ejecutable probado solo con backend local simulado, mmap solo con la unidad sintetica (no sobre Drive).
- >8 conexiones, otras horas, archivo >10 GB (limite por archivo de Drive).
- Escrituras / saves / copy-on-write (la unidad es de solo lectura por diseño).
- Persistir directorios; ajustar `file_info_timeout` y la cache de Windows; overlay de espera; UI; multiples cuentas.
- Licencia: WinFsp y winfsp-rs son GPL-3.0; un producto propietario necesita licencia comercial de WinFsp.

---
## Escrituras copy-on-write, espera de 20.5 s y preparacion de Steam (2026-10-30, noche)

### Copy-on-write local (hecho, verificado)
- Base remota inmutable + capa local persistente separada de la cache (nunca se expulsa). Copia a local perezosa (solo al escribir; truncar no descarga; borrar un remoto de 20 GB no descargo nada). fsync al cerrar un archivo modificado. Detalle: `docs/write-architecture.md`.
- `scripts/cow-test.ps1`: escritura (10/10), remontaje tras `kill` con cache vacia (6/6 persisten), capa nueva vacia => base remota intacta (4/4), verificacion de lectura sin regresion.
- Defecto real encontrado por las pruebas: WinFsp entrega nombres en MAYUSCULAS en `rename` en volumenes sin distincion de mayusculas; la busqueda remota distinguia mayusculas => renombrar un archivo remoto no ocultaba el original. Corregido (busqueda insensible por componente + nombre normalizado).
- Defecto propio evitado antes de probar: `write`/`read_directory` asincronos hacian `block_on` dentro de tokio (panico); ahora van en `block_in_place`.
- Operaciones al estilo Steam sobre la unidad de prueba `T:` (fija, NTFS): arbol de directorios, preasignar 50 MB + escrituras desordenadas + flush, manifiesto via temp+rename, mover `downloading`->`common`, `ReplaceFileW` con copia, bloqueos de rango, borrado recursivo: todo OK. Esto NO prueba que Steam acepte la unidad: eso requiere probar con Steam.

### La espera de 20.5 s
- Simulador calibrado, lectura completa de 300 MB con `Get-FileHash` (patron del incidente): configuracion antigua max 1.5 s, nueva 1.9 s, hash identico => la logica del motor no la reproduce.
- Drive real, misma lectura (285 MB) con registro de peticiones lentas: peor lectura 4.6 s, peticion mas lenta 5.2 s, media de peticiones de 8 MB 1.9 s; 8 peticiones >3 s. No se reprodujo una de 20 s (2 lecturas completas en total, 1 con el incidente).
- Conclusion: la causa mas probable es la cola de latencia de Drive (peticiones individuales varias veces mas lentas que la media); no confirmada al 100 % porque no se reprodujo.
- Mitigacion implementada y probada: (1) "hedging": si una peticion de la que depende una lectura bloqueada tarda mas de max(3 s, 2x media), se lanza un duplicado y gana el primero (solo criticas/promovidas, para no gastar cuota en especulativas); (2) timeout por peticion 30 s (antes 120 s) con reintento. Prueba con atasco simulado de 20 s: lector servido en <2 s; mutacion (sin hedging) => lector bloqueado 20 124 ms. Ahora hay registro permanente `[slow-read]`/`[slow-fetch]`/`[hedge]`.
- Nuevas metricas: lecturas bloqueadas y tiempo total bloqueado; peticiones duplicadas y cuantas ganaron.

### Pendiente
- Escritura hacia Drive (write-back por bloques): disenado, no implementado. Requiere volver a autorizar con el permiso `drive.file`.
- Risk of Rain: Steam solo permite una ubicacion de instalacion por juego; instalarlo en la biblioteca de prueba exige moverlo o desinstalarlo de la biblioteca principal => decision del usuario.
- 17 pruebas automaticas del motor + `cow-test.ps1` + `verify-mount` en verde tras todos los cambios.

---
## Volumen por bloques con subida progresiva a Drive (2026-10-30, noche)

### Arquitectura (implementada: `crates/volume`, frente WinFsp `crates/mount/src/vol.rs`, binario `sfs-volmount`)
- Archivo = tamano + mapa disperso bloque(1 MiB) -> {sucio en slab local | remoto (objeto, offset, crc32, longitud)}; huecos = ceros (preasignar no cuesta nada).
- Escritura -> slot del slab local (copy-on-write; escritura parcial sobre bloque remoto = lectura-modificacion-escritura).
- Bloques sellados (completos, al cerrar, por inactividad o por presion) se empaquetan por BYTES en objetos inmutables de 16 MiB (muchos archivos pequenos comparten objeto; un archivo de 53 bytes cuesta 53 bytes) y se suben en segundo plano. Drive solo admite objetos enteros: nunca se intenta reescribir un rango remoto.
- Un slot local se libera SOLO tras md5 verificado por Drive + commit en el journal con fsync. Un commit solo aplica si el bloque sigue apuntando a ese slot (datos mas nuevos ganan). Objetos sin bloques vivos se borran (GC).
- Contrapresion: con la cache llena, las escrituras esperan (en tareas async, no en hilos de WinFsp; timeout de IRP ampliado a 10 min).
- Durabilidad: `FlushFileBuffers` y cierre de archivo modificado = durable en SSD local (slab + journal fsync). "Subido" = durable en Drive. Reapertura tras fallo: journal reproducido, bloques sucios re-encolados.
- Espacio presentado a Windows/Steam: cuota libre real de Drive (S: = 5110 GB); el SSD lo protege la cache acotada, no el espacio mostrado.
- Escrituras a Drive espaciadas (1 objeto/s por defecto) para respetar ~3 escrituras/s por cuenta.

### Evidencia
| prueba | resultado |
|---|---|
| 512 MiB desordenado + no alineado, cache 16 MiB (test) | slab max 16 MiB, contenido identico; amplificacion de subida 1.99x (peor caso), 1.64x almacenado por fragmentacion |
| 256 MiB secuencial no alineado (patron probable de Steam) | amplificacion 1.000x, 0 RMW |
| subida lenta 32 MB/s, cache 8 MiB | escrituras frenadas al ritmo de la subida (3.4 s), sucio max 8 MiB |
| fallos de subida + "crash" + reapertura | nada liberado sin confirmacion; datos intactos; subida reanudada |
| 200 archivos pequenos | 149 300 bytes subidos = su tamano exacto, 4 objetos (antes 50) |
| montaje real, remoto simulado 40 MB/s, cache 64 MB | Copy-Item de 1.5 GB: slab local max 64 MB; robocopy de Risk of Rain; tras remontar con cache de lectura vacia: SHA256 identicos |
| montaje real sobre DRIVE, cache 32 MB | Risk of Rain (96 MB) copiado: 7 objetos, 350 esperas por contrapresion (71.6 s), slab max 32 MB; tras remontar: 31/31 SHA256 identicos leidos desde Drive |

### Limitaciones reales (no ocultas)
- TU SUBIDA: la herramienta `sfs-auth upload` (archivo por archivo, 68 peticiones) dio 0.55 MB/s; el volumen (packs de 16 MB, 2 en paralelo) dio en el monitor de red una media de 1.46 MB/s y un pico de 2.63 MB/s. Instalar desde Steam a Drive ira a ese ritmo (la contrapresion frena a Steam): a ~2 MB/s, 100 GB ~= 14 h. El SSD queda acotado; el tiempo depende de tu linea y no se puede inventar. Sin descargas de Steam simultaneas en esa ventana (sin congestion).
- Peor caso desordenado+no alineado: hasta 2x de subida; falta compactacion de packs y extents sub-bloque.
- Metadatos solo en el journal local (aun no se sube una instantanea a Drive): perder el SSD = perder el indice de los objetos (los datos estan en Drive pero sin mapa).
- Atributos/fechas explicitas no se persisten. Hard links, ADS, sparse real: no soportados (fallan limpio).
- Steam en S: aun NO probado (no se declara nada hasta ejecutarlo).

### Revision adversarial del volumen (14 hallazgos) y correcciones
Una revision independiente encontro defectos reales en mi codigo; dos se reprodujeron con pruebas que FALLABAN antes de corregir:
- #1 (reproducido: datos perdidos) un bloque completo podia subirse como 1 byte si el tamano del archivo aun no estaba actualizado (lo introduje al subir solo bytes validos; robocopy/Copy-Item lo ocultaban porque preasignan). Corregido: el tamano crece por bloque antes de sellar.
- #7 (reproducido: destino borrado) un renombrado rechazado borraba el destino. Corregido: todas las comprobaciones antes de cualquier cambio.
- #2 escritura perdida si el subidor sellaba el bloque entre la decision y la marca de escritura -> misma seccion critica.
- #3 el GC podia borrar un objeto antes de que el journal que justifica borrarlo fuera durable -> fsync antes de borrar.
- #4 un slot se reutilizaba antes de que su liberacion fuera durable (tras un crash, un archivo podia leer bytes de otro) -> los slots liberados solo se reutilizan tras la siguiente sincronizacion.
- #5 fallo de fsync en un commit -> el volumen se detiene (nunca re-encola un commit que podria llegar a disco).
- #6 lecturas fijan los objetos que leen (el GC espera). #8 la reproduccion se detiene en la primera linea corrupta; error de escritura del journal = fatal. #10 fin de archivo resuelto bajo el candado; borrar-al-cerrar por inodo. #11 GC reintenta. #12 refcount u32. #13 remapeo del mismo slot. #14 la raiz no se puede mover.
- Efecto secundario detectado por la prueba de amplificacion (1.078x) y corregido: la presion contaba mal los slots pendientes.

### Indice en Drive (antes: solo en el SSD)
- Cada 2 min (si hubo cambios) y al terminar una sincronizacion se sube una instantanea de metadatos con los bloques YA confirmados en Drive; se conservan las 2 mas recientes.
- Si el estado local desaparece, el volumen se reconstruye desde la ultima instantanea (prueba: borrar todo el estado local -> arbol y datos identicos).
- El GC no borra objetos referenciados por esas 2 instantaneas.
- Lo que no cubre: datos aun no subidos en el momento de perder el SSD.

### Estado de pruebas
volumen 9/9, motor 18/18, Drive 6/6. Montaje real: remoto simulado 1.5 GB (slab max 64 MB, SHA256 igual, 1628 MB remotos = datos exactos); Drive real: Risk of Rain 92 MB en 8 objetos (antes 108 MB por relleno), slab max 32 MB, 31/31 identicos.

---
## PRIMERA INSTALACION REAL DESDE STEAM A DRIVE (2026-10-30, 20:59-21:15) — Risk of Rain (2013) en S:
(El usuario desinstalo RoR de C: e instalo en S:. Partidas: respaldo propio con hash + Steam Cloud restauro Save.ini identico al original.)
| paso | resultado | evidencia |
|---|---|---|
| Steam acepta S: como biblioteca | SI | captura del usuario; Steam muestra 5 TB libres (cuota de Drive) |
| Instalacion directa en S: | SI | appmanifest StateFlags=4, SizeOnDisk 96189607, buildid 1134831 (= original) |
| Archivos correctos | 31/31 SHA256 identicos al original | comparacion de hashes leidos desde Drive |
| Datos en Drive | 92.3 MB en 11 objetos (22 tras jugar), 0 errores, 0 403/429 | stats del volumen |
| Uso local durante la instalacion | slab de escritura max 108 MB (limite 512 MB); C: libre 82.8 GB | volmon |
| Reconocido tras reiniciar Steam | SI | usuario |
| Ejecucion | SI, lanzado por steam.exe, imagen S:\SteamLibrary\...\Risk of Rain.exe | gamewatch.log (lista de DLL incompleta: proceso de 32 bits visto desde PowerShell de 64) |
| Partida/configuracion | Prefs.ini creado (256 B) y Save.ini escrito en S:, subidos a Drive (0 pendiente) | volumen |
| Verificar integridad (Steam) | "35 archivos validados", casi instantaneo (datos en cache de lectura por mi verificacion previa) | usuario |
| Kill del montaje + remontaje con cache de lectura vacia | 33/33 archivos identicos (juego + Prefs/Save + manifiesto), 115 s leyendo 92 MB desde Drive | prueba 8 |

Defecto detectado en la prueba real y corregido: tras subir, se descartaba la copia local y Steam ("Verificando") releia todo desde Drive. Ahora lo subido se siembra en la cache de lectura (acotada, desechable); prueba nueva: relectura con 0 peticiones remotas (mutacion sin la siembra -> falla). Desplegado tras la prueba 8.

Lo que este juego NO demuestra: con 96 MB no hizo falta contrapresion durante la instalacion (cabe en la cache de 512 MB). La contrapresion esta probada con Risk of Rain en cache de 32 MB y con 1.5 GB en cache de 64 MB, pero aun no con un juego de Steam mayor que la cache. No se afirma nada sobre juegos de 100 GB.

---
## INCIDENTE 2026-10-30 ~21:36 — perdida de la copia de Risk of Rain en S: (datos de PRUEBA; nada personal perdido)
Que paso (reconstruido con la instantanea remota de las 21:31 y el journal):
1. Tras la prueba "kill + remontaje" (correcta: 33/33 identicos), Steam vio reaparecer S: y empezo a revalidar/reescribir la instalacion (creo downloading/248820, temp/248820, un manifiesto nuevo).
2. Mate el montaje (taskkill) para recompilar, con Steam a mitad de esas operaciones.
3. El journal local NO se sincronizaba por si solo (solo en commits/GC/flush) y un renombrado con reemplazo eran 2 registros: el bufer de 8 KB se vuelca al llenarse, asi que pudo persistir "borrar destino" sin "mover". Al reabrir, los 28 archivos movidos por Steam y el manifiesto desaparecieron del indice.
4. Al reabrir, el GC borro de Drive los packs que quedaron sin referencia: la proteccion "no borrar lo que referencia la instantanea remota" solo existia dentro de la sesion que la subio. ESTE fue el paso irreversible.
Impacto: ~96 MB de datos de la instalacion de prueba borrados de Drive. Save.ini del usuario intacto (en Drive, respaldo local y Steam Cloud). Kingdom Come no se habia tocado.
Correcciones (con pruebas que reproducen el incidente y comprobacion por mutacion):
- Operaciones de varios registros = UNA linea del journal (J::Batch): rename-over y truncados son todo-o-nada. Prueba: linea cortada a la mitad -> estado anterior intacto.
- Group commit: el journal se sincroniza cada 200 ms aunque nadie haga flush. Prueba: renombrado sin flush, copia del estado como tras un kill -> presente. Mutacion sin el sincronizador -> falla.
- GC conservador: los objetos sin referencia AL ABRIR nunca se borran automaticamente; al abrir se carga la proteccion de las 2 instantaneas remotas mas nuevas; se compara el arbol local con la instantanea remota y se avisa (missing_vs_remote). Opcion --restore-from-remote. Prueba: perder el indice local de un archivo subido -> 0 objetos borrados, aviso, restauracion con contenido identico. Mutacion con el GC antiguo -> falla.
- Regla operativa: no desmontar S: con Steam usandola; para recompilar, primero pausar/cerrar la descarga en Steam.
Estado de S: tras el incidente: indice consistente con los objetos vivos; 8 objetos huerfanos (ya inexistentes en Drive) quedan registrados sin uso; los restos de Risk of Rain en S: (Save.ini, Prefs.ini) no afectan; Steam ya no lo considera instalado alli.
Pruebas del volumen: 16/16.

---
## Backend VPS Contabo (2026-10-30/31)
- Contratado por el usuario: Storage VPS 10, 1 mes, US-East, Ubuntu 24.04 — 7,05 EUR pagados (pedido 15618284), sin extras. 290 GB utilizables.
- Bastionado y despliegue: ver C:\dev\contabo-ops\OPERATION_LOG.md. Solo el puerto 22 abierto; sfs-server escucha en 127.0.0.1 y se usa por tunel SSH.
- Nuevo codigo: crates/server (sfs-server: lecturas por rango, objetos atomicos con fsync+md5, listados, espacio), crates/http (Backend + ObjectStore, compatible con el motor y el volumen), http-probe. Google Drive intacto.
- Mediciones (ida y vuelta 135 ms):
  - disco del VPS: 1.7 GB/s secuencial, 8586 IOPS 4K aleatorio (no es el cuello de botella)
  - red del VPS hacia internet: 20-23 MB/s (puerto 200 Mbit/s)
  - un flujo TCP PC<-VPS: 2.58 MB/s con CUBIC; 6.03 MB/s con BBR (cambio aplicado)
  - tunel por el ssh de Git (MSYS): 1.5 MB/s y colapsa con paralelismo (16 peticiones -> 0.1 MB/s); tunel por OpenSSH nativo de Windows: 7.6 MB/s (64 MB, CON una descarga ajena del usuario en curso) -> se usa el nativo
  - ufw "limit 22" rechaza >6 conexiones/30 s: "muchos tuneles" no es viable (es la proteccion funcionando)
- PENDIENTE: benchmark limpio (sin descargas ajenas) por el tunel nativo; comparacion con Drive; montaje S:/volumen sobre el VPS; compresion adaptativa (RAW/LZ4/ZSTD 1-3) DESPUES de la referencia limpia; SteamCMD (requiere decision del usuario por el riesgo del token de sesion).

## Clean baseline 2026-10-31 (idle link, native OpenSSH tunnel, BBR on VPS)
Contabo via SSH tunnel (http-probe, n=30 per size, random offsets in 2 GiB random file):
| size | p50 | p95 | p99 |
|---|---|---|---|
| 256K | 267 ms | 441 ms | 700 ms |
| 1M | 586 ms | 2143 ms | 3968 ms |
| 4M | 1027 ms | 4157 ms | 6283 ms |
| 16M | 2755 ms | 9374 ms | 12669 ms |
stat p50 304 ms. Sequential 1 stream (8 MB req) 2.6 MB/s. Parallel 4 MB: 2 conns 11.2, 4 conns 4.9, 8 conns 4.7, 16 conns 4.6 MB/s.
Upload 16 MB object 0.27 MB/s (home uplink), round trip identical. 286 req, 0 errors.
Drive (drive-probe, n=6): 256K p50 1151 / p95 1565; 1M 1362/1901; 4M 2649/4238; 16M 2597/3237 ms.
Concurrency 2 MB: 1 conn 2.6, 2: 4.6, 4: 9.0, 8: 3.5 MB/s. 0 x 403/429.
Reading: Contabo wins small blocks (256K 4.3x faster p50) -> better for blocking reads; Drive has tighter tails at 16M.
Bottleneck: single SSH TCP connection multiplexes all requests -> >2 concurrent requests collapse (11.2 -> 4.7 MB/s) and tails explode.
Next candidates: N parallel SSH connections (ufw limit 6/30s), or authenticated non-SSH transport.

## Wire compression sfz1 (2026-10-31)
Study (compbench, byte-weighted random blocks, real files, PC CPU):
- Project Zomboid 1 MiB: zstd1 ratio 0.522, lz4 0.592, zstd3 0.519; 64% of blocks gain >5%. lotpack 0.12, xml/txt 0.1-0.16; bank/png/bik/zip ~1.0 (already compressed).
- PZ 256 KiB: zstd1 0.514 (no loss vs 1 MiB). RoR: zstd1 0.832 (ogg 1.0, data.win 0.70).
- VPS CPU (2x EPYC, /usr/lib sample): zstd1 enc 239 MB/s, dec 466 MB/s per core. PC: zstd1 enc ~510, dec ~1000 MB/s.
- Delivery model per 1 MiB block, rtt 131 ms: p50 raw 535 -> zstd1 191 ms @2.6 MB/s; 225 -> 146 @11.2; 184 -> 140 @19.75. p95/p99 unchanged (tail = incompressible blocks). zstd2/3 no better than zstd1; adaptive oracle ~= zstd1+RAW fallback.
Decision: zstd1 per 256 KiB chunk, RAW if gain <5%, compressed LRU cache on server (256 MB), one compress permit (try_lock: busy -> RAW, never waits), client decodes on one blocking worker, CRC32 + bounded sizes.
Verification: codec unit tests (roundtrip, fallback, malformed, bomb, frame stream); http-verify 600 random unaligned reads byte-exact raw vs sfz1 (wire 0.44 of raw); cache hits on reread; sfs-mount --backend http (--root /media) over local sfs-server: 303 files / 1.59 GB SHA256 identical. Local loopback: busy_raw ~50% of pieces under parallel fetch (expected; real link pending).
NOT YET MEASURED: end-to-end over the real VPS link (deferred: KCD download running, no network benchmarks during it).

## KCD on Contabo via SteamCMD (2026-10-31)
User authenticated SteamCMD personally (Steam flagged datacenter login twice; third attempt approved via Steam's "it was me / Steam client / server location / other" form).
Download 06:21-07:43 UTC: 63.6 GB download, 72.6 GB on disk, 314 files, ~13-15 MB/s (100-115 Mbps) steady. `app_update 379430 validate` -> "Success! App '379430' fully installed.", exit 0, StateFlags 4.
Token kept on VPS by user decision (config.vdf now 600, dir 700). SHA256 manifest -> /var/lib/streamfs/kcd-sha256.txt (in progress).
Compression on real KCD data (VPS, 800 x 256 KiB samples): .pak = 99.7% of bytes, zstd1 ratio 0.951 (lz4 0.968), only 29% of blocks gain >5%; delivery time unchanged at every bandwidth (p50 144 ms @19.75 MB/s raw and zstd1). dll 0.49.
=> KCD is already compressed: sfz1 falls back to RAW, gains nothing, costs ~7% of one VPS core on first read of each chunk (cached as incompressible afterwards). Compression is a win for games like PZ (0.51), neutral for KCD.

## Real-link measurements after changes (2026-10-31, link idle, BBR)
Multi-tunnel (4 native SSH tunnels, HttpStore round-robin, supervisor scripts/tunnel.ps1). Mixed test: 256 KiB blocking reads (n=40) while 4 streams of 8 MB reads run; 3 interleaved rounds:
| | 1 tunnel | 4 tunnels |
|---|---|---|
| 256K p50 | 408 / 401 / 402 ms | 335 / 332 / 321 ms |
| 256K p95 | 777 / 3137 / 1600 ms | 917 / 728 / 757 ms |
| background | 17.3 / 8.6 / 12.2 MB/s | 17.5 / 18.1 / 20.4 MB/s |
Note: same-session 1-tunnel suite was much faster than the earlier "clean" baseline (parallel 15.6 vs 4.6 MB/s): link quality varies strongly with time of day; only interleaved A/B comparisons are trusted.
sfz1 deployed on VPS (op-20261031-sfz1). Paired A/B (same file/range read raw and sfz1, order alternated, pools warmed, 120 pairs, 4 tunnels, every sfz1 read a server cache miss):
- PZ dedicated server maps (Muldraugh, anonymous SteamCMD 380870, real data): wire 12.9% of bytes; mean 515 -> 281 ms; p95 1547 -> 356 ms; p99 4064 -> 3197 ms; per-file median diff -4 ms; 0 mismatches.
- KCD Data/*.pak (1 MiB random ranges): wire 96.5%; mean 458 -> 415; p95 1094 -> 928; p99 3372 -> 2287; median diff +2 ms (noise); 0 mismatches.
=> keep sfz1 on by default.

## KCD first play from K: (2026-10-31 13:04-13:18) and crash analysis
- Boot to menu and in-menu world streaming worked; game closed when starting a new game (intro video `cin_7805_stribrna_skalice.usm` opened 13:17:05, crash 13:18:06).
- 13 min: backend 4.84 GB fetched, 2.63 GB served to the game, 3.1 GB of it speculative; 795 blocked reads; 0 backend errors; single reads up to 25 s (226 MB shadercache.pak read: 20.7 s; 1 KiB reads in Objects.pak/patch: 11 s).
- Crash: APPCRASH in WHGame.dll+0xbba175, code 0x80000003 (breakpoint = deliberate stop by the game). Minidump has no symbols/thread names; no read error on our side (all fetches ok=true).
- Evidence for the I/O hypothesis: in the 61 s between the video open and the crash the game got only ~28 MB and was blocked ~58 s (40 blocked reads). Competing hypothesis: memory pressure (game log at start: 16 GB RAM, 1.85 GB available, 88% in use). NOT confirmed; to be re-tested after the scheduler fix.

## Request scheduler v2 (2026-10-31)
Root cause measured: urgent reads queued behind speculative/bulk bytes already on the wire (slot priority alone cannot help once 7 x 8 MiB prefetches are in flight on a ~10 MB/s link).
Changes (sfs-core sched/engine/store, sfs-http, sfs-server):
- 3 classes: Urgent (blocked reader; own slot pool, never waits behind others) > Bulk (tail of large reads; <= half the slots, bytes <= 2x budget) > Spec (read-ahead/learned prefetch).
- Spec bytes in flight bounded by an adaptive budget (LEDBAT-like: urgent latency > floor+150 ms halves it, otherwise +512 KiB; 1-48 MiB). Spec requests <= 2 MiB; stale queued guesses dropped after 10 s; read-ahead backpressure (no new guesses while 24 queued).
- Large reads: next 3 windows fetched in parallel as Bulk (bounded) instead of one window per round trip.
- Urgent lane: with >= 3 tunnels, tunnel 1 carries only urgent reads (no head-of-line blocking inside one SSH TCP connection); hedges go to another lane.
- Cache: segmented LRU; guessed blocks enter on probation and are evicted before anything an app read; stats probation/spec_used/spec_evicted_unused.
- Integrity: client pins `x-sfs-version`; server answers 412 if the file changed before/after the read (op-20261031-verpin). Cancelled guesses that a reader joined are re-claimed, never surfaced as errors.
- Metrics: blocked-read p50/95/99/max, urgent queue vs transfer histograms, bulk/spec/cancelled counts, budget/cuts, tunnel_down, version_mismatch, urgent_lane (mount --stats-out).
- Learned prefetch (sfs-core learn.rs, mount default, --no-learn to disable): records reads of the first 15 min per mount into <cache>/learn.json (last 4 sessions, 1 MiB grain); next mount prefetches regions seen in >= 2 sessions (or the only session) in first-use order, as Spec, budget min(cache/4, 4 GB). No per-game rules.
- Defaults: min_fetch 1 MiB -> 512 KiB, max_fetch 8 -> 4 MiB.

engine-bench (crates/http/src/bin/engine-bench.rs): fresh cache, 2 sequential 256K streams + small random urgent reads (4-64 KiB, 150 ms think) + one 64 MiB bulk read at t=20 s, 60 s, real link, 4 tunnels, alternated runs, same seeds.
| build | urgent p50 | p95 | p99 | max | reads >1 s | urgent reads done | bulk 64 MiB | total MB/s |
|---|---|---|---|---|---|---|---|---|
| base (8 runs) | 529-1035 | 1773-5425 | 2953-17428 | 3819-17428 ms | 8-25 | 35-63 | 11-46 s | 5.6-11.5 |
| v4 = shipped (4 runs) | 445-491 | 1147-3201 | 2249-3752 | 3229-5368 ms | 7-9 | 70-81 | 12-33 s | 6.5-8.2 |
Trade-off (honest): tails 2-4x better and ~1.5x more urgent reads served, but total throughput ~25% lower (sequential streams 4.4-6.1 vs 7.7-8.7 MB/s) because the budget keeps the bottleneck queue short. Link varied strongly during the runs (base p50 529-1035 ms).
Integrity: Y: test mount with the new engine, 7 KCD files / 1.08 GB (64 MiB and 64 KiB read sizes) SHA-256 identical to the VPS record; 52 workspace tests pass.

## KCD run 2 with engine v2 (2026-10-31 14:53-15:04 local) — WARM cache (4.86 GB from run 1), learned prefetch empty
- Boot -> menu: 2.06 GB read, 99.8% cache hits (warm cache, not engine merit). New game 14:58, intro video skipped, level load, autosave permanent001.whs written 15:04:18 (game world initialised). User: "fluido" until minimising.
- Crash 15:04:25 when the user minimised the game: 0xc0000374 HEAP CORRUPTION in ntdll, faulting thread stack = window-message processing (user32, gameoverlayrenderer64 = Steam overlay, Tobii.GameIntegration, dinput8, WHGame). Different from run 1 (0x80000003 in WHGame). No I/O error: backend_errors 0, retries 0, version_changed 0, write_errors 0; saves go to C:\Users\..\Saved Games (not K:). At crash: 1.9 GB free, commit 80%. => not attributable to the network/FS on the evidence; most consistent with a game/overlay fault on minimise. Earlier in the run memory DID hit the wall: 14:55:12 free 5 MB, commit 97.5% (Chrome 8.5 GB private + game 7.8 GB).
- Engine (new data only, ~11 min): served 2.83 GB, backend 2.37 GB (spec 1.92 GB), blocked reads 963: p50 351 ms, p95 1.12 s, p99 2.44 s, max 5.5 s (run 1: max 25 s); urgent transfer p50 339 ms / p99 2.29 s; urgent lane 1199 requests; tunnel_down 0. sfs-mount RAM <= 49 MB WS / 96 MB private.
- Waste: probation (guessed, never read) 1.37 GB of 1.92 GB speculative; spec_used 0.41 GB. Cause: read-ahead started at 8 MiB and doubled on any two contiguous reads; KCD reads 128 KiB entries and jumps ~25 MB inside paks.
- Fix prepared (not yet mounted): read-ahead starts at 1 MiB and doubles only after the previous batch was actually consumed; a jump resets it.

## KCD run 3 (2026-10-31 16:58-18:05 local, ~67 min) — WARM cache (7.24 GB), adaptive read-ahead, learned prefetch 139 regions / 388 MB
Build C:/dev/sfs-target-c/run2/sfs-mount.exe (sha 6bc5b104…). Chrome stayed open (user need). Data: C:/dev/kcd3-timeline.csv + kcd3b-timeline.csv (2 s samples), kcd3-mount.log, kcd3-final-stats.json.
- Gameplay (user report): new game -> fled the attack -> rode; loaded an old near-end-game save; galloped across much of the map. "muy fluido", "10/10". Minimised once: no crash (run 2's crash on minimise did not reproduce). Closed normally from the menu: no dump, no WER event.
- Visible symptoms: textures pop in (missing -> low-res -> full), first-time map areas take a few seconds; dialog choices once shown before their text (IPL_English.pak 512 B read took 2.6 s); one audio loop at 18:00:44 = music.pak reads of 5.0 MB / 3.7 MB that took 6.6 s / 4.6 s.
- Engine: 0 errors, 0 retries, 0 version changes, 0 corrupt blocks. Served 56.8 GB, backend 2.30 GB (4%). Blocked reads 2554: p50 336 ms, p95 876 ms, p99 3.30 s, max 10.05 s (Levels/rataje/level.pak 68 KB). Urgent queue p99 3 µs (scheduler never made a demand read wait); urgent transfer p50 330 ms, p99 3.25 s. Hedges 41 (36 won by the duplicate). Budget cuts 226.
- Waste: speculative 1.03 GB in 67 min (run 2: 1.92 GB in 11 min); spec_used 136 MB; probation 702 MB. Absolute speculative traffic per minute fell ~11x; the hit ratio of guesses is still low.
- Memory: game private up to 10.3 GB (WS often ~1.7 GB: paged out); system free min 167 MB, commit max 93.2% (Chrome + Claude app open). Commit limit 41 GB (system-managed 25 GB pagefile), so not exhausted. sfs-mount private usually 20-250 MB, transient peak 657 MB at 17:29:38 while serving a ~13 GB cache-hit burst (old-save load); freed within seconds. CPU at 17:23: total 83% (game 53%, Claude app 14%, Chrome 11%); GPU 3D ~32%, game VRAM 2.25 GB.
- Disk: cache 9.1 GB of the 16 GB cap; game tree 18.3 GB; overlay 0.7 MB.

### Diagnosis of the "~3 s" requests (after the run, link idle)
- curl through the tunnels: RTT ~0.28 s; 256 KB in 0.33-0.97 s; one tunnel moves ~3.7 MB/s; 4 tunnels in parallel ~6-13 MB/s total; 8 concurrent 512 KB on one tunnel 0.4-0.7 s; a 64 KB probe on tunnel 0 while 3 tunnels pull 16 MB: 0.34-0.42 s. So no fixed 3 s cost in the transport.
- The 3.2-3.4 s cluster in the log = hedge delay (3 s) + one ~0.3 s duplicate: some primaries stall and the duplicate on another tunnel wins (36/41).
- Root cause found in code: every non-speculative request (urgent AND bulk) used the reserved urgent lane, tunnel 0 (urgent_lane 2781 of 4509 requests). All demand bytes, including multi-MB blocked reads (music.pak), went through ONE TCP connection (~3.7 MB/s) while tunnels 1-3 carried only prefetch. Large blocked reads were also issued as single 4 MiB runs with a 4 MiB bulk byte cap, i.e. one request in flight.
- Side finding: the server answers 500 (should be 416) for a range starting past EOF.

## Scheduler fix: demand traffic over all tunnels + stall recovery (2026-10-31, after run 3)
Changes (crates/core/src/engine.rs, crates/sim):
1. Lanes: the reserved urgent tunnel 0 carries only small demand requests (<= min_fetch = 512 KiB, Urgent or promoted); bulk and larger urgent runs rotate over tunnels 1-3.
2. Split: a blocked read spanning > 2 MiB, and all bulk lookahead, is fetched as ~1 MiB runs in parallel (Core.split_blocks) instead of 4 MiB runs one at a time.
3. Stall deadline (Config.stall_timeout, default 8 s): first attempt of a request is abandoned after 8 s + 1 s/MiB (bounded by request_timeout 30 s) and retried with the full timeout on another tunnel. A link that is merely slow costs at most one retry, never a failed read.
4. Hedging only for non-bulk requests (bulk recovers through 3), hedge_max_inflight 2 -> 4 (the 5% + burst-of-4 ratio cap is unchanged).
5. Bug fix: when a hedged pair raced, the first *finished* copy won even if it was an error (e.g. its deadline fired); now the first *successful* copy wins and an error waits for the other copy.

A/B method: engine-bench on the real link (2 sequential 256K streams + random 4-64 KiB urgent reads with 150 ms think + one 64 MiB blocking read at secs/3; fresh cache; 60 s). Same binary, old behaviour via env SFS_AB_OLD=1, alternating old/new, same seeds. "Urgent read" here = one Engine read() call of the benchmark (filesystem-request level), not a tunnel request. Synthetic benchmark, not a KCD session.
Final round (v10 = all changes except 5), 6 pairs, medians (old -> new):

| metric | old | new |
|---|---|---|
| 64 MiB blocking read | 15.4 s | 9.0 s |
| sequential streams | 6.2 MB/s | 7.0 MB/s |
| urgent p50 | 390 ms | 363 ms |
| urgent p95 | 1100 ms | 685 ms |
| urgent p99 | 2499 ms | 2204 ms |
| urgent max (median of runs) | 6253 ms | 3264 ms |
| urgent > 1 s (pooled) | 33/525 (6.3%) | 29/538 (5.4%) |
| backend bytes / served | 1.123 | 1.113 |
| corrupt blocks / errors surfaced | 0 / 0 | 0 / 0 |

Worst single read in the new arm: 13.5 s (hedged pair both stalled, the primary's deadline error was returned while the duplicate was still running -> fixed by 5). Intermediate rounds: v6 (lanes+split only) and v9 (+stall 8 s, no bulk hedge, 2 hedge slots): bulk ~1.8x faster in both, but v9 urgent p99/max were worse than old (3.2 s / 5.0 s vs 2.7 / 3.5) because bulk hedges used up the 2 hedge slots; that is why 4 was added. A stall deadline of 4 s (v7) made a link that was slow everywhere worse (restarts of requests that would have finished) -> 8 s.
Recovery validation:
- Fault-injection tests (sfs-sim, new): `stalled_piece_of_a_large_read_is_retried_without_refetching_the_rest` (8 MiB read, one 1 MiB piece hangs 60 s, deadline 0.3 s): 1.41 s, 1 retry, fetched 8.65 MB for 8.39 MB asked (only the stalled piece again). `flaky_link_with_stalls_and_resets...` (every 7th request hangs 60 s, every 11th resets, 6 parallel readers, 60 reads 4 KiB-6 MiB): all bytes exact, 7.8-8.9 s, 89.8 MB asked -> 108.5 MB fetched (1.21x incl. read-ahead), 24 retries / 151 requests, 11 hedges. Whole workspace: 54 tests pass.
- Real data: Y: test mount with the new build (C:/dev/sfs-target-c/run3, sha 8091d984...), fresh cache: 7 KCD files / 1.08 GB read with 64 MiB and 64 KiB chunks, SHA-256 7/7 identical to the VPS manifest; backend bytes == bytes served (1,082,077,762), 0 retries, 0 errors; GameData.pak 10.1 MB/s, shadercache.pak 11.1 MB/s.
- Not yet verified: a KCD session with this build (needs the user), behaviour during a real tunnel drop with the new deadlines (the 18:37 drop happened while only benchmarks ran; the bench panics on errors by design and the mount was idle).

## Read-size / latency-breakdown instrumentation (2026-10-31, build run4 sha 2cf4a2ec...)
Units, kept separate everywhere: (a) *read* = one Handle::read call = one filesystem read request reaching K: (after the Windows cache manager; the game's own ReadFile calls are not visible from the FS); (b) *backend request* = one range request sent through a tunnel (block-aligned, 256 KiB blocks, runs up to 1-4 MiB).
- engine stats: `sizes[]` per read-size bucket (<4K, 4K-64K, 64K-1M, 1M-4M, 4M-16M, 16M-64M, >=64M): reads, bytes, blocked reads, blocked_ms_sum, latency p50/p95/p99/max for cache hits vs reads that waited for remote data; `blocked_wall_ms` = union of blocked intervals (wall time with >= 1 read frozen on I/O, parallel waits not added up); `req_queue_ms_total` (scheduler wait, all classes incl. speculative); `store_ms_total` (client cache insert).
- http stats, split [<=512 KiB, larger] backend requests: ranges, ttfb_ms_sum (request -> response headers: RTT + server work), body_ms_sum (receiving the body), server_read_ms_sum (pread on the VPS), server_total_ms_sum (all server work), decode_ms (sfz1).
- sfs-server v3 deployed (op-20261031-srvtiming) to report x-sfs-read-us / x-sfs-srv-us.
- First SYNTHETIC check (engine-bench seed 61, not KCD): per backend request avg small/large: server disk read 1.8/4.2 ms, all server work 5.0/17.6 ms, ttfb 288/485 ms, body 171/471 ms -> VPS disk and CPU are negligible; time is network (RTT ~0.28 s + transfer). blocked_wall 60.1 s vs blocked sum 183.6 s over a 60 s run (parallel waits overlap ~3x).
- KCD numbers: pending a real session with run4 on K:.

## KCD session 4 (2026-10-31 21:17:42-21:33:26 local, ~16 min) — real game, build run4, WARM cache 9.1 GB, learned prefetch 137 regions
Phases: A low graphics 21:17-21:27 (incl. a fast travel to an unvisited area), B max graphics 21:27-21:32 (settings change + reload + new areas), C low again until exit. Clean exit from the menu, no dump/WER. zstd level 1, no config change during the session. Data: kcd4-final-stats.json, kcd4-phaseA-low.json, kcd4-phaseB-high.json, kcd4-timeline.csv, kcd4-mount.log.
Totals: 172,116 reads (filesystem read requests), 33.5 GB served, 764 MB from the VPS (spec 458 MB, of which used 88 MB; probation 170 MB). 1,332 backend requests (621 spec, 20 bulk). Errors reaching the game 0; engine backend_errors 3 (speculative, not retried), retries 0, hedges 7 (4 won), corrupt 0, version changes 0.
Reads by size (filesystem level):

| size | reads | % reads | % bytes | waited (blocked) | % of all wait | hit p50 / p99 | miss p50 / p95 / p99 / max |
|---|---|---|---|---|---|---|---|
| <4K | 9,683 | 5.6 | 0.02 | 3 | 1.3 | 75 us / 0.7 ms | 603 ms / 3.3 s / 3.3 s / 3.3 s |
| 4K-64K | 141,587 | 82.3 | 3.7 | 438 (0.3%) | **56.5** | 78 us / 0.8 ms | 305 ms / 937 ms / 2.0 s / 4.6 s |
| 64K-1M | 20,281 | 11.8 | 7.4 | 388 (1.9%) | **39.7** | 166 us / 1.4 ms | 250 ms / 687 ms / 2.5 s / 3.3 s |
| 1M-4M | 138 | 0.08 | 0.6 | 2 | 0.6 | 1.9 ms / 6.6 ms | 684 ms / 1.3 s |
| 4M-16M | 19 | 0.01 | 0.3 | 3 | 1.9 | 8.4 ms / 22 ms | 1.9 s / 3.4 s |
| 16M-64M | 29 | 0.02 | 3.4 | 0 | 0 | 44 ms / 75 ms | - |
| >=64M | 379 | 0.22 | **84.6** | 0 | 0 | 58 ms / 307 ms | - |

- Blocking: 834 reads waited (0.48% of reads), sum 324.0 s, wall-clock union 320.4 s -> the game almost never has two reads waiting at once: its I/O is serial, so each miss costs a full round trip.
- When: 87% of the wait time falls in 2-s samples that were >75% blocked, i.e. sustained loading bursts (fast travel 21:22:43-21:23:31 = 47 s of blocking; graphics change + reload 21:28:55-21:30:30 = 85 s). Outside those bursts gameplay waits were short and rare (user: "fluido", no freezes; textures pop in late at first).
- Per backend request (avg, small <=512 KiB / large): ttfb 232 / 252 ms (~= link RTT), body 177 / 342 ms, VPS disk read 2.5 / 4.2 ms, all VPS work 5.8 / 12.1 ms. Client: urgent scheduler queue p99 2 us, sfz1 decode 0.3 s and cache insert 1.4 s for the whole session. => the VPS disk, VPS CPU, scheduler and client CPU are negligible; a blocked small read = RTT (~230 ms) + receiving the 512 KiB min_fetch (~180 ms).
- Graphics max vs low: in 5 min, 439 MB from the VPS (358 MB of it speculative) vs 313 MB in the 10 min before; game private RAM max 10.6 GB, free RAM min 540 MB, commit max 88%; blocked p99 for the whole session 2.5 s (run 3: 3.3 s), max 4.6 s (run 3: 10 s).

### Where the real margin is (ranked by impact on what the player feels)
1. Serial misses during loading bursts (small/medium reads = 96% of wait). Each costs ~0.3-0.4 s and they come one after another. Levers: issue the likely next reads in parallel before the game asks (structure-aware: ZIP central directory gives every entry's exact offset/size, so a read that lands on an entry can fetch the whole entry / the next entries; behaviour-based: co-access sequences learned across sessions). This attacks round trips, not bytes.
2. Body time of a small miss (~180 ms for 512 KiB at single-connection speed). A/B min_fetch 512 -> 256 KiB for isolated random reads (est. -70 to -90 ms per small miss) against the risk of more misses on neighbouring reads.
3. Tail: hedge delay 3 s is above p99 (2.5 s). Hedging small urgent reads at ~1-1.5 s (within the 5% cap) targets the 2.5-4.6 s waits.
Low impact here: large reads (>= 1 MiB = 2.5% of wait), compression (bytes are not the bottleneck; zstd 3 to be A/B'd separately), VPS disk/CPU, scheduler queueing.
Speculation efficiency is still low: 458 MB speculative, 88 MB of it used (19%).
