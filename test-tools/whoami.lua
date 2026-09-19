-- desc: 返回当前时间与主机名（host 辅助函数）
-- schema: {"type":"object","properties":{}}
-- 主机名跨平台获取：Linux 读 /etc/hostname；macOS/Windows 上该文件
-- 不存在，回退到 HOSTNAME 环境变量（多数 shell 都导出它），再回退
-- 到 Windows 的 COMPUTERNAME，最后 'unknown'。pcall 保证读取失败
-- 不会炸掉整个工具调用。
local function hostname()
    local ok, text = pcall(host.read_file, '/etc/hostname')
    if ok and text and #text > 0 then
        return text:gsub('%s+', '')
    end
    local env_host = host.env('HOSTNAME')
    if env_host and #env_host > 0 then
        return env_host
    end
    local computername = host.env('COMPUTERNAME') -- Windows
    if computername and #computername > 0 then
        return computername
    end
    return 'unknown'
end
return function(args)
    return string.format('time=%s host=%s', host.date('%Y-%m-%d %H:%M:%S'), hostname())
end
