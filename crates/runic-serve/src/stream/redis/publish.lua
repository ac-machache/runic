local payload = ARGV[1]
local weight = tonumber(ARGV[2])
local budget = tonumber(ARGV[3])
local ttl = tonumber(ARGV[4])
local channel = ARGV[5]
local run_id = ARGV[6]

local seq = redis.call('INCR', KEYS[2])
redis.call('XADD', KEYS[1], seq .. '-0', 'w', weight, 'p', payload)
local held = redis.call('INCRBY', KEYS[3], weight)

while held > budget do
    local oldest = redis.call('XRANGE', KEYS[1], '-', '+', 'COUNT', 1)
    if #oldest == 0 then
        break
    end
    local fields = oldest[1][2]
    local dropped = 0
    for index = 1, #fields, 2 do
        if fields[index] == 'w' then
            dropped = tonumber(fields[index + 1])
        end
    end
    redis.call('XDEL', KEYS[1], oldest[1][1])
    held = redis.call('DECRBY', KEYS[3], dropped)
end

redis.call('EXPIRE', KEYS[1], ttl)
redis.call('EXPIRE', KEYS[2], ttl)
redis.call('EXPIRE', KEYS[3], ttl)
redis.call('PUBLISH', channel, run_id)

return seq
