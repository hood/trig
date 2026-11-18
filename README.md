# trig.nvim

Outrageously fast indexed code search for Neovim. Trigram index in RocksDB, Rust core over LuaJIT FFI, NEON in the hot paths.

Trig keeps a trigram index, so a query only reads files that can actually match, then verifies them with the same line semantics as the almighty `ripgrep` (❤️).

| linux kernel, 97k files, M1 Pro, warm cache | rg | trig |
|---|---|---|
| `spin_lock_irqsave` (15.9k hits) | 2295ms | 63ms |
| `xa_(load\|store)\(` | 2304ms | 20ms |
| `hello world` | 2236ms | 12ms |
| full index | | 10s, 86MB |
| reindex `kernel/sched/core.c` on save | | 6.5ms |

(Result sets match `rg -S --hidden` exactly on the queries above)

## Install

Needs Rust, cmake and `libclang` (Xcode CLT on macOS). First build compiles RocksDB, takes a few minutes.

```sh
git clone https://github.com/hood/trig.nvim ~/src/trig.nvim
cd ~/src/trig.nvim && cargo build --release
```

Load it from the clone, since the native lib lives in `target/release`:

```lua
vim.opt.rtp:append(vim.fn.expand("~/src/trig.nvim"))
vim.cmd.runtime("plugin/trig.lua")
vim.keymap.set("n", "F", "<cmd>TrigLive<cr>")
```

`:TrigLive` needs [fzf.vim](https://github.com/junegunn/fzf.vim).

## Usage

| | |
|---|---|
| `:TrigLive [query]` | live search in fzf, re-queries the index per keystroke |
| `:Trig {regex}` | results into quickfix (`:Trig!` = fixed string) |
| `:TrigIndex [dir]` | build/refresh the index |
| `:TrigStatus` | index state and DB path |

The git root of cwd gets indexed on startup. After the first run only files whose mtime/size changed are re-read. Saved buffers are re-indexed on `BufWritePost`. Smart case like `rg -S`, `.gitignore` respected, hidden files included, `.git` skipped.

Options:

```lua
require("trig").setup({
  auto_index = true,
  live_max = 2000,  -- hits per keystroke in :TrigLive
  qf_max = 10000,
})
```

There's also a CLI, which `:TrigLive` uses under the hood:

```sh
trig index                      # in a repo; DB goes to ~/.cache/nvim/trig/<hash>
trig search --stats 'foo_(bar|baz)'
trig stats
trig compact
```

## How it works

- **Index**: every file is reduced to its set of ASCII-case-folded trigrams (dedup through a 2^24-bit bitset). Posting lists are sorted `u32` doc ids, Stream VByte encoded over deltas.
- **Writes**: a changed file gets a new doc id. Postings are appended through a RocksDB merge operator, so saving a file never reads the old lists. The old id goes into a tombstone bitmap. The merge operator and a compaction filter drop tombstoned ids. `trig compact` folds everything and clears the bitmap.
- **Queries**: the regex is parsed with `regex-syntax` and turned into an AND/OR trigram query, e.g. `ab(c|d)e` becomes `(abc AND bce) OR (abd AND bde)`. Posting lists are intersected, then candidates are verified in parallel with `regex`.
- **NEON**: sorted-set intersection (4x4 block compare, galloping when sizes are skewed), Stream VByte decode with in-register prefix sum, newline counting, lowercasing. Scalar fallbacks for other architectures.

## Limits

- Files changed outside nvim (e.g. `git checkout`) are picked up on the next startup or `:TrigIndex`, there's no watcher yet.
- While the first index builds, searches only see what's already been flushed.
- Binary files (NUL in the first 8KB) and files over 64MB aren't indexed.
- Only tested on macOS/aarch64.
