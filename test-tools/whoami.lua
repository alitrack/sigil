-- desc: 返回当前时间与主机名（os/io 标准库 + host 辅助函数）
-- schema: {"type":"object","properties":{}}
return function(args)
    local hostname = host.read_file('/etc/hostname'):gsub('%s+', '')
    return string.format('time=%s host=%s', os.date('%Y-%m-%d %H:%M:%S'), hostname)
end
