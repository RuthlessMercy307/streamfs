# StreamFS

Juega en Windows juegos de PC que están guardados en un servidor remoto (un VPS), **sin descargarlos antes**. StreamFS monta la carpeta remota como una unidad de Windows (por ejemplo `K:`). Steam la reconoce como una biblioteca más, y el juego lee solo los trozos que necesita mientras juegas. Lo que se usa se guarda en una caché local con un límite de tamaño, así que la segunda vez ya no se descarga.

> Proyecto experimental, hecho y medido con un único juego y una única configuración (Kingdom Come: Deliverance, VPS en Europa, cliente en Sudamérica, ~0,28 s de ida y vuelta). Los números de abajo son de esa configuración, no una promesa.

## Cómo funciona

```
Juego ──> K: (WinFsp) ──> motor sfs-core ──> caché local (bloques de 256 KB, CRC32)
                                │
                                └── 4 túneles SSH ──> sfs-server en el VPS ──> archivos del juego
```

**Motor (`sfs-core`)**
- **Planificador por prioridades.**
  - Lo que el juego está esperando va siempre primero.
  - Las lecturas grandes se reparten en trozos de 1 MB por varios túneles.
  - La precarga solo usa el ancho de banda que sobra, con un control adaptativo tipo LEDBAT que la frena cuando las lecturas urgentes se vuelven lentas.
- **Recuperación ante fallos.**
  - Si un pedido se cuelga, se corta a los 8 s más 1 s por MB y se repite por otro túnel.
  - Las lecturas pequeñas lentas se piden también por otro túnel (*hedging*) y gana la primera respuesta correcta.
  - Nunca se entregan datos a medias ni de otra versión del archivo: el servidor rechaza con 412 si el archivo cambió.
- **Caché local.**
  - Cada bloque lleva su suma de verificación (CRC32).
  - Lo descargado por adivinación entra "a prueba" y es lo primero que se borra si nunca se usó.
- **Aprendizaje.** Recuerda qué se leyó al arrancar en sesiones anteriores y lo precarga.

**Otras piezas**
- **Escrituras del juego** (logs, configuración): van a una capa local de copia en escritura. Los archivos remotos nunca se modifican.
- **`sfs-server`**: solo escucha en `127.0.0.1` del VPS. Se llega a él por túneles SSH, con cifrado y clave, y además pide un token. No hay nada expuesto a internet.
- **`sfs-overlay`**: panel con estadísticas en vivo encima del juego. Se abre y se cierra con **Ctrl+Alt+F12**.

## Resultados medidos (Kingdom Come: Deliverance, ~18 GB)

| | Primera prueba | Versión actual |
|---|---|---|
| Espera más larga del juego | 25 s, y el juego se cerró | 4,6 s |
| El 99 % de las esperas, por debajo de | — | 2,5 s |
| Datos servidos desde el PC (sesión de 16 min) | — | 99,5 % de las lecturas |
| Bajado del servidor en esa sesión | — | 0,76 GB, sobre 33,5 GB leídos por el juego |
| Espacio local usado | — | ~10 GB de caché (límite configurable) |

Lo que se ha visto en las pruebas:
- Arrancar el juego, partida nueva, cinemática, viaje rápido a zonas sin visitar y gráficos al máximo, sin congelones largos.
- La verificación SHA-256 de archivos leídos a través de la unidad salió idéntica al original.

El detalle de cada medición está en [docs/DEVLOG.md](docs/DEVLOG.md).

**Dónde se va el tiempo:** casi toda la espera son lecturas pequeñas (menos de 1 MB) que no estaban en caché, y el juego las pide de una en una durante las pantallas de carga. Cada una cuesta lo que tarda un viaje de ida y vuelta al servidor. La compresión no ayuda con estos datos, porque los paquetes del juego ya vienen comprimidos ([estudio](docs/COMPRESSION_RESEARCH.md)).

## Requisitos

- **En tu PC:** Windows 10/11 con [WinFsp](https://winfsp.dev/) instalado y el cliente OpenSSH de Windows.
- **Servidor:** un VPS Linux con acceso SSH por clave y espacio para tus juegos.
- **Para compilar:** [Rust](https://rustup.rs/). Para compilar el servidor desde Windows: `cargo-zigbuild` + `zig`, o compila directamente en Linux.

## Puesta en marcha (resumen)

1. **Servidor (VPS)**
   ```bash
   cargo build --release -p sfs-server --target x86_64-unknown-linux-musl
   # copia el binario a /opt/streamfs/sfs-server y crea un token aleatorio
   head -c 32 /dev/urandom | base64 > /etc/streamfs/token && chmod 600 /etc/streamfs/token
   /opt/streamfs/sfs-server --files /srv/juegos --objects /srv/streamfs-obj --token-file /etc/streamfs/token --listen 127.0.0.1:8787
   ```
   Lo recomendable es ejecutarlo como servicio systemd con un usuario propio. Deja abierto **solo** el puerto SSH.
2. **Juegos en el VPS:** copia la carpeta de la biblioteca de Steam (`SteamLibrary/steamapps/...`) dentro de `--files`. Usa juegos que sean tuyos; StreamFS no evita ninguna protección anticopia.
3. **Túneles (PC):** `scripts/tunnel.ps1 -HostName <tu-vps> -User <usuario>` mantiene 4 túneles SSH abiertos y los reconecta solos.
4. **Montar la unidad (PC):**
   ```powershell
   cargo build --release -p sfs-mount -p sfs-overlay
   sfs-mount --backend http --url http://127.0.0.1:8787,http://127.0.0.1:8788,http://127.0.0.1:8789,http://127.0.0.1:8790 `
     --token-file <ruta-del-token> --root /<carpeta> --mount K: --cache-dir C:\streamfs-cache --cache-mb 16384 --stats-out C:\streamfs-cache\stats.json
   ```
   Copia `winfsp-x64.dll` junto a `sfs-mount.exe`.
5. **Steam:** *Configuración → Almacenamiento → Añadir unidad → K:*. Si Steam pide "redistribuibles comunes de Steamworks", instálalos.
6. **Overlay:** `sfs-overlay --stats C:\streamfs-cache\stats.json --drive K:`. Después, **Ctrl+Alt+F12** lo muestra y lo oculta.
   - El overlay es una ventana aparte: no se inyecta en el juego.
   - Se ve con el juego en **ventana sin bordes**. En pantalla completa exclusiva queda tapado.
   - Para que aparezca al grabar con OBS, usa *captura de pantalla* en lugar de *captura de juego*.

## Límites conocidos

- La primera vez que se carga una zona nueva del juego hay esperas de entre 0,3 y 4 s por lectura. Dependen de la distancia al servidor.
- La precarga aprendida solo cubre lo que se repite entre sesiones. Cada juego tiene que "aprenderse" desde cero.
- La unidad es de solo lectura para los archivos remotos. Las partidas guardadas suelen ir a `Documentos`/`Saved Games`, que no está en la unidad.
- Probado solo en Windows 11 con un juego. Úsalo bajo tu propia responsabilidad.

## Estructura

| Carpeta | Contenido |
|---|---|
| `crates/core` | Motor: planificador, caché, precarga, aprendizaje, estadísticas, registro de lecturas |
| `crates/mount` | Unidad de Windows (WinFsp) y capa de copia en escritura |
| `crates/http` | Cliente del servidor (varios túneles, compresión sfz1) y `engine-bench` |
| `crates/server` | `sfs-server` para el VPS |
| `crates/codec` | Formato de compresión por trozos (sfz1/zstd) |
| `crates/sim` | Backend simulado, pruebas de fallos y `trace-replay`, que reproduce sesiones grabadas |
| `crates/overlay` | Overlay de estadísticas en vivo |
| `crates/gdrive`, `crates/volume` | Backend experimental de Google Drive y volúmenes de escritura |
| `scripts/` | Túneles SSH y monitores de prueba |

## Licencia

MIT. Ver [LICENSE](LICENSE).
