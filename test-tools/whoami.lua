-- desc: 返回当前时间与主机名（host 辅助函数）
-- schema: {"type":"object","properties":{}}
return function(args)
    local hostname = host.read_file('/etc/hostname'):gsub('%s+', '')
    return string.format('time=%s host=%s', host.date('%Y-%m-%d %H:%M:%S'), hostname)
end
