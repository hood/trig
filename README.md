# trig

Outrageously fast indexed code search. Trigram index in RocksDB, Rust core, NEON in the hot paths.

Trig keeps a trigram index, so a query only reads files that can actually match, then verifies them with the same line semantics as the almighty `ripgrep` (❤️).

| linux kernel, 97k files, M1 Pro, warm cache | rg | trig |
|---|---|---|
| `spin_lock_irqsave` (15.9k hits) | 2295ms | 63ms |
| `xa_(load\|store)\(` | 2304ms | 20ms |
| `hello world` | 2236ms | 12ms |
| full index | | 10s, 86MB |
| reindex `kernel/sched/core.c` on save | | 6.5ms |

(Result sets match `rg -S --hidden` exactly on the queries above)

Ships as a CLI and as `libtrig`, a C ABI lib for editor integrations. Neovim plugin: [trig.nvim](https://github.com/hood/trig.nvim).

## Install

Needs Rust, cmake and `libclang` (Xcode CLT on macOS). First build compiles RocksDB, takes a few minutes.

```sh
git clone https://github.com/hood/trig ~/src/trig
cd ~/src/trig && cargo build --release
```

`target/release/` then holds the `trig` binary and `libtrig.{dylib,so}`.

## Usage

```sh
trig index                      # in a repo; DB goes to ~/.cache/nvim/trig/<hash>
trig search 'foo_(bar|baz)'     # path:line:col:text, like rg --vimgrep
trig search --stats -F 'a.b'    # fixed string, plan + timings on stderr
trig update src/main.rs         # reindex one file
trig stats
trig compact
trig lib-path                   # where libtrig lives, for integrations
```

Smart case like `rg -S`, `.gitignore` respected, hidden files included, `.git` skipped. After the first `index` only files whose mtime/size changed are re-read.

## C ABI

`crates/ffi` builds `libtrig`: open a handle per root, index in the background, update single files, search into a `path:line:col:text` buffer. Every entry point catches panics. `trig_abi_version()` is bumped on breaking changes so integrations can refuse a mismatched lib.

## How it works

- **Index**: every file is reduced to its set of ASCII-case-folded trigrams (dedup through a 2^24-bit bitset). Posting lists are sorted `u32` doc ids, Stream VByte encoded over deltas.
- **Writes**: a changed file gets a new doc id. Postings are appended through a RocksDB merge operator, so saving a file never reads the old lists. The old id goes into a tombstone bitmap. The merge operator and a compaction filter drop tombstoned ids. `trig compact` folds everything and clears the bitmap.
- **Queries**: the regex is parsed with `regex-syntax` and turned into an AND/OR trigram query, e.g. `ab(c|d)e` becomes `(abc AND bce) OR (abd AND bde)`. Posting lists are intersected, then candidates are verified in parallel with `regex`.
- **NEON**: sorted-set intersection (4x4 block compare, galloping when sizes are skewed), Stream VByte decode with in-register prefix sum, newline counting, lowercasing. Scalar fallbacks for other architectures.

## Limits

- No file watcher yet: changes from outside an integration (e.g. `git checkout`) are picked up on the next `trig index`.
- While the first index builds, searches only see what's already been flushed.
- Binary files (NUL in the first 8KB) and files over 64MB aren't indexed.
- Only tested on macOS/aarch64, x86/Linux support is WIP.
