# StreamFS wire compression: KCD paks (research report)

Date 2026-10-31. Data: 364 MiB of local samples (`samples/manifest.json`, SHA-256 per range) taken from 8 KCD paks: tails holding the central directory, plus interior 16 MiB ranges that include the exact ranges of the earlier wire probe. All work was offline and single-threaded on a Ryzen 5 3600, using Inkscape's CPython 3.12 with zstandard 0.25 (libzstd 1.5.7), zlib 1.3.1, lzma, bz2, numpy, and the brotli 1.2 CLI. lz4 is not installed; zstd -5 stands in for it. Every transform and codec was round-tripped and checked byte-exact with SHA-256. No failures.

**Labels.** **[M]** = measured on the samples. **[E]** = estimated with a model, using the link figures supplied: RTT 0.28 s, 3.7 MB/s per tunnel, 10.5 MB/s for large engine reads, and a VPS CPU assumed to run at 0.6x this PC.

## Verdict (short)

1. **The data is already compressed. We are not missing redundancy.**
   - [M] Buildings, Cloth-part0, Textures and Objects are **100 % deflate (method 8) entries**:
     - Buildings: 30,242 entries (64 % `.dds.N` mips, 36 % `.cgf`).
     - Cloth: 98.5 % `.dds.N`.
     - Textures: 100 % DDS.
     - Objects: 74 % `.dds.N`, 25 % `.cgf`.
   - [M] On their interior samples, raw-byte entropy is 7.97-7.99 bits/byte, and zstd 1/3/19, lzma6, bz2 and brotli all land at **0.990-1.000**.
   - [M] Exact duplicates are zero: 0 aligned 4 KiB / 64 KiB duplicate blocks within or across files. Content-defined chunking finds 0.04 % duplicate bytes.
   - [M] A trained zstd dictionary gains 0-0.1 %.
   - [M] Stored entries are already-compressed media:
     - English.pak: 100 % stored `.ogg` (Vorbis); zstd ≈ 0.86-0.88 on interior ranges.
     - music.pak: 99.9 % stored `.sqcb`; ≈ 0.95-0.96.
     - Videos: ≈ 0.90-0.93.
2. **The one compressible thing in the pak bytes is stored terrain in level.pak.**
   - [M] `level.pak` holds `terrain/terrain.dat` (283 MiB, stored), `terrain/cover.ctc` (85 MiB, stored) and 142 k small deflated `.dat` entries.
   - [M] On cover.ctc, 256 KiB blocks compress to: zstd1 **0.903**, zstd3 **0.828**, zstd19 0.789, lzma6 0.719.
   - [M] The central directories are 0.7-13.7 MiB per pak and compress to 0.12-0.18 with zstd3. sfz1 (per-256 KiB zstd) already captures that.
3. **sfz1 behaves like zstd level ~1.** [M] On the exact probe ranges, my zstd1 numbers match your sfz1 numbers to within about 1 point. Level ~1 is an inference from that match, not a reading of the engine config:

   | Range | sfz1 | my zstd1 | my zstd3 |
   |---|---|---|---|
   | level.pak @ 41943040 | 91 % | 0.906 | 0.835 |
   | English | 88 % | 0.879 | 0.867 |
   | music | 97 % | 0.962 | 0.961 |
   | Buildings / Cloth | 100 % | 1.000 / 0.999 | — |

4. **Recompressing the deflate entries (precomp style) works in principle, but does not lower game wait time.** Details in T4.

## Measured results

### ZIP structure [M] (`results/zip_summary.json`)

| pak | size | entries | method by bytes | main content |
|---|---|---|---|---|
| Buildings | 1.93 GB | 30,242 | deflate 100 % | `.dds.N` 64 % (deflate ratio 0.77), `.cgf` 36 % (0.48) |
| Cloth-part0 | 1.94 GB | 6,635 | deflate 100 % | `.dds.N` 98.5 % (0.58) |
| Textures | 0.60 GB | 30,143 | deflate 100 % | `.dds.N` 77 % / `.dds` 23 % (0.75-0.81) |
| Objects | 1.69 GB | 46,838 | deflate 100 % | `.dds.N` 74 %, `.cgf` 25 % |
| level.pak (rataje) | 0.73 GB | 148,697 | stored 55 % / deflate 45 % | terrain.dat + cover.ctc stored; 142 k tiny `.dat`, 1.8 k `.xml` deflate |
| English | 1.82 GB | 75,521 | stored 100 % | `.ogg` |
| music | 0.37 GB | 140 | stored 100 % | `.sqcb` |

Further facts [M]:
- No entries are encrypted.
- Entry sizes are mostly small (median 4-13 KiB), but most bytes sit in large entries. Share of bytes in entries above 256 KiB: Buildings 61 %, Cloth 91 %, Objects 43 %, Textures 43 %.
- The textures are BC1, BC3, BC4, BC5 and BC7. The `.dds.N` mips have no header, so they take the format of their parent `.dds`.

### Fair codec comparison, every 256 KiB block of every sample [M] (`results/fair_matrix.json`)

Each block is sent compressed only if it shrinks.

- **Across all 364 MiB:** zstd-5 0.911, zstd1 0.897, zstd3 0.891, zstd19 0.881, lzma6 0.876.
  - That average is inflated by the tail samples, because they contain the central directories.
- **Speed on this PC, single thread:**

  | Codec | compress | decompress |
  |---|---|---|
  | zstd-5 | 2.7 GB/s | 15 GB/s |
  | zstd1 | 957 MB/s | 4.8 GB/s |
  | zstd3 | 584 MB/s | 4.0 GB/s |
  | zstd19 | 8.3 MB/s | 2.0 GB/s |
  | lzma6 | ~5 MB/s | ~72 MB/s |
  | zlib6 | ~38 MB/s | ~390 MB/s |

- **Block size** (`block_stats.json`): moving from 64 KiB to 1 MiB or 16 MiB blocks changes ratios by about 1-3 points, and only on the compressible samples. Note that this file's per-codec ratios use different random block subsets per codec, so read `fair_matrix` for codec-to-codec comparisons.

### Per-entry content [M] (`results/entry_analysis.json`, `results/precomp_bench.json`)

- Every inflated entry passed its CRC32 check (4,753 entries).
- Columns below: **ratio vs deflate** = recompressed inflated payload ÷ original deflate bytes in the pak. **zstd19 on pak bytes** = what any wire codec gets today.

  | pak (sample) | zstd19 on pak bytes | zstd19 of inflated | lzma6 of inflated | zstd3 of inflated |
  |---|---|---|---|---|
  | Buildings | 0.991 | 0.885 | **0.751** | 0.996 |
  | Cloth-part0 | 0.997 | 0.880 | **0.788** | 1.007 |
  | Textures | 0.999 | 0.983 | 0.956 | 1.027 |
  | Objects | 0.998 | 0.940 | **0.872** | 1.024 |
  | level.pak deflate entries | 0.985 | 0.792 | 0.751 | 0.883 |

- **Exact deflate reproduction: 100 % of the deflate entries tested** were rebuilt byte-for-byte by stock zlib 1.3.1 with raw deflate (wbits -15) and default strategy:
  - Texture and mesh paks: level 6, memLevel 8 or 9.
  - level.pak: levels 2, 6, 8 and 9, with memLevel 5, 8 or 9.
  - Entries above 4 MiB were not tested.
  - My first run reported 4,007 failures in level.pak; that was a bug in my parameter list, not a property of the data. With the widened search there were 0 failures.
- **Client re-deflate speed** (zlib, this PC, single core): **8-25 MB/s** (textures/meshes 15-25 MB/s, level9 xml ~5 MB/s). This is the cost that decides the precomp question.
- **BCn component split** (endpoints and indices stored separately, then zstd19), relative to the inflated data:

  | Format | plain zstd19 | after split |
  |---|---|---|
  | BC3 (typical) | 0.369 / 0.405 / 0.794 | 0.331 / 0.355 / 0.717 |
  | BC1 (typical) | 0.556 / 0.610 / 0.699 | 0.507 / 0.561 / 0.643 |
  | BC5 | — | gains 0 to 3 points |
  | BC7 | 0.37 | 0.42 (worse) |

  So the split adds roughly 4-10 % on top of recompression, and only for BC1/BC3.
- **Stride transpose / delta on `.cgf` meshes:** no gain (0.385 → 0.41 at best, i.e. worse).
- **cover.ctc:** an 8-byte stride transpose improves zstd19 from 0.796 to 0.731.

## Techniques: one verdict each

| # | Technique | Result | Verdict |
|---|---|---|---|
| T1 | On-the-fly zstd per 256 KiB block (sfz1 today) | Saves 0 % on texture/mesh paks [M]. Small savings on ogg / music / video / level terrain. CPU cost is negligible (zstd3 < 1 ms per block) [M]. | **Keep.** It must skip blocks that don't shrink (it seems to). |
| T2 | Raise the sfz1 level from ~1 to 3 | cover.ctc 0.903 → 0.828; English 0.879 → 0.867; other ranges ±0.5 % [M]. Costs about 0.3 ms more server CPU per 256 KiB [M/E]. | **The only cheap win, and it is small.** [E] A 64 KiB read at 3.7 MB/s is ~5 ms faster (~1.5 % of p50 336 ms); a 1 MB read ~20 ms. Worth a one-line change if the VPS CPU has headroom; it does nothing for Buildings or Cloth. |
| T3 | Pre-compressed block store (zstd19 / lzma6 per 256 KiB, built once on the VPS) | Over zstd3 it adds ~4 points on terrain and ~1 point elsewhere [M]. lzma6 decode costs 3.6 ms per block [M], which cancels its gain at 10.5 MB/s [E]. | **Not worth it** for latency (≤ 2-4 % on level data only). Only worth it for VPS egress volume, and then only for compressible stored entries (level terrain ≈ 0.37 GB per level → ~0.29 GB stored). |
| T4 | Precomp: inflate deflate entries, store as lzma/zstd19 (± BCn split), client re-deflates the exact bytes | Bytes −25 % on Buildings, −21 % Cloth, −13 % Objects, −4 % Textures, −25 % level deflate entries [M]. Re-deflate 8-25 MB/s per core [M]. Works only per whole entry. | **Do not integrate for blocked reads.** [E] A 1 MiB Buildings entry: raw 380 ms at 10.5 MB/s vs 442-454 ms with precomp; at 3.7 MB/s, 563 ms vs 591-604 ms (only ~500 ms if re-deflate fully overlaps the transfer). Other blockers below. |
| T5 | Dedup (fixed or content-defined chunks, within or across files) | 0 % / 0.04 % [M] | **Useless** on these paks. |
| T6 | Trained zstd dictionary | ≤ 0.1 % on paks; 1-5 % on ogg/music at 64 KiB blocks [M] | **Not worth it.** |
| T7 | Stronger codecs (lzma, bz2, brotli 11) on the raw pak bytes | Within 1 % of zstd19 on deflate paks [M]. brotli11 decodes at 1.6-18 MB/s from the CLI (spawn overhead included). | **No.** |
| T8 | BCn split / mesh transforms | +4-10 % only on top of T4 (BC1/BC3); 0 on meshes [M] | **Only meaningful together with T4**, so no. |

Why T4 fails even though it shrinks bytes:
- **Client CPU.** Re-deflating with zlib runs at 8-25 MB/s per core, the same order as the link (10.5 MB/s). Whatever transfer time it saves, the client spends rebuilding the stream.
- **Whole-entry granularity.** To return bytes from the middle of an entry, the client has to re-deflate from the entry's start. Restarting mid-stream would need deflate-state checkpoints (preflate-style), which is a large job. Most bytes sit in entries above 256 KiB, while game reads are 4-128 KiB.
- **zlib version lock.** The client must run byte-identical zlib 1.3.x deflate, with the exact level and memLevel recorded per entry. Rust's miniz_oxide produces different output.
- **Storage.** The VPS has to keep an extra 75-96 % copy of each pak, because Steam's original files are never replaced.

The only case where T4 could pay off is bulk background prefetch. There, re-deflate would run on idle cores in parallel (~6 × 15 MB/s), and the link becomes the limit. [E] Effective throughput would rise by up to +33 % for Buildings-like data, +15 % for Objects and +5 % for Textures. It would compete with the game for CPU and requires all the machinery above. This is research-grade only; I don't recommend it now.

## Where the time actually goes [E]

These estimates combine the supplied measurements with the model above:
- A 64 KiB blocked read is one 256 KiB block: ≈ 280 ms RTT + 71 ms transfer at 3.7 MB/s, or 25 ms at 10.5 MB/s.
- So **RTT is 80-92 % of a small read**. Even perfect 2:1 compression would save only 12-35 ms, against a p50 of 336 ms.
- p95 (876 ms) and p99 (3.3 s) are queueing and tail effects that compression cannot fix.

Better levers, outside this research's scope: fewer round trips (prefetch from the pak central directory: entry offsets/sizes are known exactly), entry-aligned fetches (fetch whole entry spans instead of fixed 256 KiB blocks, since the game inflates whole entries), and request parallelism/pipelining.

## VPS disk vs network vs wait time [E]

- **T2:** 0 extra disk. Network −0 to 7 % on stored-media ranges (more on the KCD session mix only where level terrain, ogg or video dominate; ~0 on texture/mesh paks). Wait time −0 to 1.5 % at p50.
- **T3:** extra disk ≈ 0.8 × the size of the compressible stored data. Network about 2-4 points better than T2 on terrain only. Wait time about the same as T2.
- **T4:** extra disk ≈ 0.75-0.96 × the pak size. Network −4 to −25 % on deflate paks. Wait time **worse** for blocked reads.

## Is anything worth integrating?

- **Yes, a small one:** raise sfz1 from zstd ~1 to level 3 (or adaptive 3/1 depending on VPS CPU load). Keep skipping blocks that don't shrink. The gain is limited to stored compressible data (level terrain, ogg, video): a few ms per read.
- **No:** dictionaries, dedup, lzma/brotli, a pre-compressed store, or precomp/BCn transforms.
- **Answer to the question:** the texture/mesh paks are incompressible because they are already deflate-compressed. We are not failing to exploit structure. The redundancy that remains inside the deflate streams (deflate vs lzma, about 4-25 %) is real, but it can only be reclaimed with exact zlib re-deflate on the client, which costs as much time as it saves.

## Reproduce

Run from `scripts/` (Python run with `-I`):
1. `zip_index.py`
2. `entry_analysis.py --max-bytes 50331648 --transform-max 8388608`
3. `block_stats.py --budget-mb 12 --slow-budget-mb 3 --brotli`
4. `fair_matrix.py`
5. `precomp_bench.py --cap-mb 16`
6. `latency_model.py` — its output uses the biased per-codec subsets from `block_stats`; the [E] figures above come from `fair_matrix` instead.

`selftest.py` checks the whole pipeline on a synthetic ZIP. Logs and JSON are in `results/`.

**Limits:**
- Samples cover 364 MiB of about 11 GB of paks.
- Entries above 4 MiB were not tested for deflate reproduction.
- Not sampled: `terrain.dat`, the Videos central directory, and the exe/dll/shadercache/Sounds_HD files behind the measured 15 % on the mixed read.
- VPS CPU speed is assumed (0.6x this PC), not measured.
