local ffi = require("ffi")

ffi.cdef([[
typedef struct TrigHandle TrigHandle;
typedef struct { int32_t state; uint64_t done; uint64_t total; } TrigStatus;

const char *trig_last_error(void);
TrigHandle *trig_open(const char *root, const char *db);
void trig_close(TrigHandle *h);
const char *trig_db_path(const TrigHandle *h);
const char *trig_root(const TrigHandle *h);
int32_t trig_index_async(const TrigHandle *h, int32_t compact);
void trig_status(const TrigHandle *h, TrigStatus *out);
int32_t trig_update(const TrigHandle *h, const char *path);
int32_t trig_remove(const TrigHandle *h, const char *path);
uint8_t *trig_search(const TrigHandle *h, const char *pattern, uint32_t flags, uint32_t max, size_t *out_len);
void trig_free(uint8_t *p, size_t len);
]])

local M = {}

-- <plugin>/lua/trig/native.lua -> <plugin>
M.plugin_root = vim.fn.fnamemodify(debug.getinfo(1, "S").source:sub(2), ":p:h:h:h")
M.bin_dir = M.plugin_root .. "/target/release"

local ext = jit.os == "OSX" and "dylib" or "so"
local ok, lib = pcall(ffi.load, M.bin_dir .. "/libtrig." .. ext)
M.lib = ok and lib or nil
M.load_error = not ok and lib or nil

M.FLAG_IGNORE_CASE = 1
M.FLAG_CASE_SENSITIVE = 2
M.FLAG_FIXED = 4

M.STATE = { [0] = "idle", "walking", "indexing", "compacting", "ready", "error" }

function M.last_error()
  return ffi.string(M.lib.trig_last_error())
end

function M.status(h)
  local st = ffi.new("TrigStatus[1]")
  M.lib.trig_status(h, st)
  return { state = M.STATE[st[0].state] or "?", done = tonumber(st[0].done), total = tonumber(st[0].total) }
end

function M.search(h, pattern, flags, max)
  local len = ffi.new("size_t[1]")
  local p = M.lib.trig_search(h, pattern, flags or 0, max or 0, len)
  if p == nil then
    return nil, M.last_error()
  end
  local s = ffi.string(p, len[0])
  M.lib.trig_free(p, len[0])
  return s
end

return M
