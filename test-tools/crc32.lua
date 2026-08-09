-- desc: 计算字符串的 CRC32 校验和（通过 LuaJIT FFI 调用系统 zlib，无 Rust 重编译）
-- schema: {"type":"object","properties":{"text":{"type":"string","description":"要计算 CRC32 的文本"}},"required":["text"]}
local ffi = require('ffi')
ffi.cdef[[
    unsigned long crc32(unsigned long crc, const unsigned char *buf, unsigned int len);
]]
local zlib = ffi.load('z')
return function(args)
    local text = args.text or ''
    local buf = ffi.new('unsigned char[?]', #text)
    ffi.copy(buf, text, #text)
    local crc = zlib.crc32(0, buf, #text)
    return string.format('0x%08X', crc)
end
