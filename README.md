# ***lightspeed*** ⚡

A speed-first C++ indexer and lightweight language server. Definitions, references, rename, hover, completion, call/type hierarchy, semantic tokens and document links, driven by `compile_commands.json`.

```
cargo build --release
target\release\lightspeed lsp --cdb path\to\compile_commands.json   # speaks LSP on stdio.
target\release\lightspeed lsp --ninja path\to\build.ninja  # tries it best to parse ninja directly

# add --fuzzy-refs to include uncertain references (deep templates etc)
```

## Performance

Cold full index, no cache, using every hardware thread.

| | AAA game engine | [Godot](https://github.com/godotengine/godot) |
|:--|--:|--:|
| Machine | Threadripper PRO 7975WX (64 threads) | Ryzen 7 7800X3D (16 threads) |
| Files indexed | 75,542 | 7,098 |
| Symbols | 4,286,248 | 442,432 |
| Parallel analysis | 54.5 s | 3.75 s |
| Serial merge | 1.1 s | 0.05 s |
| Indexes + hierarchy | 1.6 s | 0.05 s |
| **Total** | **62.1 s** | **3.85 s** |
| Peak memory | 15.0 GiB | 411 MiB |
| Symbol DB | 394 MiB | 41 MiB |

CPU time by phase, summed over all threads:

| Phase | AAA game engine | Godot |
|:--|--:|--:|
| Preprocess (incl. lexing and directive scanning) | 2,647 s | 36.7 s |
| File load | 299 s | 20.6 s |
| Parse, identifiers, using/alias scan | 10.4 s | 0.8 s |

Templates, specializations and type inference are lazy and cost nothing at index time.

## Design and tradeoffs

- Headers are indexed once, on their own, under the flags of one including TU. Inside a TU they're only scanned for `#define`s and conditionals. So `#if`-dependent declarations that differ per configuration show up for only one of them.
- Function bodies and annotation macros (`UCLASS(...)`) stay opaque, so there's no type checking or overload resolution. Results are heuristic, and ambiguous references are reported as *uncertain*.
- Tolerant over correct: unknown `#if` identifiers are `0`, and malformed code is skipped rather than rejected.
- Hash-validated on-disk cache of analysis results.

Design notes live in the module docs (`src/lib.rs` lists the layers).
