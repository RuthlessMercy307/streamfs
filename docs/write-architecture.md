# Escrituras: capa copy-on-write local (hecha) y write-back por bloques hacia Drive (diseño)

Fuentes y evidencia: `../../docs/research-steam-library-drive-writes.md`.

## 1. Lo que ya existe y está probado (capa local)
- Base remota inmutable + capa local persistente (`%LOCALAPPDATA%\streamfs\overlay\<volumen>`), separada de la caché de bloques y **nunca expulsada**.
- Copia a local perezosa: solo en la primera escritura; un `overwrite`/truncado a 0 no descarga nada; borrar un archivo remoto no lo descarga (whiteout persistido de forma atómica).
- `fsync` al cerrar un archivo modificado y en cada `FlushFileBuffers`.
- Operaciones: crear/escribir/escribir aleatorio/truncar/extender/renombrar (con y sin reemplazo)/`ReplaceFileW`/borrar/mkdir/rmdir (no vacío rechazado)/bloqueos de rango/varios handles.
- Pruebas: `scripts/cow-test.ps1` (escritura → remontaje tras `kill` → capa vacía con base intacta) y la simulación de operaciones de Steam sobre `T:`.
- Límites conocidos: los atributos y fechas explícitas no se persisten; renombrar un **directorio** que tiene contenido remoto devuelve ACCESS_DENIED (requeriría copia recursiva); el espacio libre reportado es el del disco local que guarda la capa (honesto: ahí cae hoy cada byte escrito).

## 2. Por qué no basta para "instalar en Drive"
Hoy todo lo escrito queda en el SSD local. Una instalación de 60 GB necesitaría 60 GB locales. Hace falta subir a Drive y liberar el espacio local, con estas restricciones (verificadas en documentación oficial):
- Drive **no** admite escribir un rango de un archivo existente: `files.update` reemplaza el contenido entero.
- Escrituras sostenidas: ~3 peticiones/s por cuenta (no ampliable); 750 GB/día de subida por usuario; 500 000 hijos por carpeta.
- Subida reanudable en trozos múltiplos de 256 KiB; sesión válida 1 semana.

## 3. Diseño: almacén de bloques con write-back acotado
**Formato remoto (en una carpeta privada de la app, scope `drive.file`):**
- Los datos de los archivos escritos por la unidad se guardan como **objetos de 32 MiB** (`pack/<hh>/<id>`), cada uno inmutable, que contienen *extents* de 4 MiB de uno o varios archivos (los archivos pequeños se empaquetan juntos para no gastar una petición por archivo).
- **Metadatos** (árbol de directorios, tamaño, lista de extents por archivo `offset→(objeto, desplazamiento, longitud, sha256)`): base SQLite local con journal WAL + instantánea comprimida subida periódicamente a Drive (`meta/snapshot-<n>`), para remontar en otro PC.
- Renombrar/mover (incluido `downloading → common` de Steam) = **solo metadatos**, sin tocar datos remotos.
- Una escritura sobre un extent ya subido crea un extent nuevo (copy-on-write); el viejo pasa a basura y un GC compacta objetos con poca ocupación.

**Caché de escritura local acotada (no 100 GB):**
- Los datos escritos van a extents "sucios" en el SSD (tope configurable, p. ej. 2–4 GiB).
- Un subidor en segundo plano empaqueta extents completos en objetos de 32 MiB y los sube (reanudable). Al confirmarse la subida (y su sha256), el extent pasa a "limpio": se puede expulsar como cualquier bloque de lectura.
- **Contrapresión**: si la caché sucia llega al tope, las escrituras esperan a que la subida libere espacio. La instalación de Steam irá a la velocidad de subida, pero nunca llenará el SSD.
- Las lecturas de datos aún no subidos salen de la caché sucia; las de datos subidos usan el motor de lectura actual (rangos sobre los objetos), sin cambios en él.

**Política de persistencia (explícita):**
| Momento | Garantía |
|---|---|
| `write` devuelto | datos en la caché local (memoria del SO) |
| `FlushFileBuffers` / cierre de archivo modificado | **durable en el SSD local** (fsync de datos + journal de metadatos). Sobrevive a cierre del programa, cuelgue o corte de luz. |
| Estado "subido" (visible en la UI) | durable en Drive: objeto subido y verificado + metadatos en instantánea remota |
Nunca se responde a un flush antes de que los datos estén en el disco local. Lo que **no** cubre el nivel local: la pérdida o el robo del propio SSD antes de que termine la subida (como cualquier disco). Opción `--sync-remote` para que el flush espere a Drive (lento; solo para pruebas o datos críticos).

**Errores y recuperación:**
- Subida fallida: reintento con backoff; las sesiones reanudables se retoman con `PUT bytes */total`.
- 403 de cuota diaria (750 GB) o de velocidad: la subida se pausa, los datos siguen durables en local, la contrapresión frena las escrituras nuevas y la UI lo explica; nunca se descartan datos.
- Arranque tras un fallo: se reproduce el journal; los extents sucios se vuelven a encolar; los objetos remotos huérfanos (subidos pero sin metadatos) los recoge el GC.

## 4. Steam encima de esto
- Descarga/instalación: escribe en `downloading/`, mueve a `common/` (renombrado de metadatos). Espacio local necesario ≈ tope de la caché sucia, no el tamaño del juego.
- Actualizaciones: Steam construye la versión nueva junto a la vieja y luego mueve/borra (documentación de Steamworks). En Drive eso cuesta el tamaño de los archivos que cambian, no el del juego; en local, solo el tope de la caché sucia.
- "Verificar archivos": lee el juego entero → descarga todo lo que no esté en caché (coste de red completo; es inherente).
- Espacio libre reportado a Steam: `min(espacio libre de Drive, cuota configurada)`; Steam comprueba el espacio antes de instalar.

## 5. Plan de implementación (en orden, cada paso con pruebas)
1. `drive.file` + subida reanudable en `sfs-gdrive` (requiere volver a autorizar: añade el permiso de crear archivos propios de la app).
2. Metadatos SQLite + extents + caché sucia acotada + contrapresión, primero contra un backend de objetos simulado (y S3 después).
3. Subidor en segundo plano con empaquetado de 32 MiB, verificación sha256 y reanudación.
4. GC/compactación.
5. Steam: añadir biblioteca, instalar, reiniciar Steam, ejecutar, guardar, verificar, desmontar/remontar.
