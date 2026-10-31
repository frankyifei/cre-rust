--[[--
qkinput.koplugin -- optional input bridge for the qemu-kindle harness.

The harness shows the real Kindle framebuffer in a window and writes mouse
interaction into a FIFO (see tools/qemu-kindle).  Under qemu-user the
synthetic evdev device is read by libkoreader-input but the Lua gesture layer
does not turn those events into gestures, so this plugin injects gestures one
layer higher: it polls the FIFO and broadcasts standard KOReader "Gesture"
events (tap / swipe), which is exactly what the reader's touch zones consume.

It is inert unless QK_TAP_FIFO is set, so it is safe to leave installed.

FIFO protocol (same as the viewer writes):
    d X Y   touch down
    m X Y   touch move
    u       touch up
    t X Y   complete tap
]]

local UIManager = require("ui/uimanager")
local Event = require("ui/event")
local Geom = require("ui/geometry")
local time = require("ui/time")
local WidgetContainer = require("ui/widget/container/widgetcontainer")
local logger = require("logger")

local ffi = require("ffi")
ffi.cdef[[
int open(const char *pathname, int flags, ...);
long read(int fd, void *buf, unsigned long count);
int close(int fd);
]]

local O_RDONLY = 0
local O_NONBLOCK = 2048

local POLL_INTERVAL = 0.08        -- seconds between FIFO polls
local SWIPE_THRESHOLD = 40        -- pixels of travel to classify as a swipe
local MAX_TAP_MS = 1200           -- generous: qemu/UI latency is high

local QkInput = WidgetContainer:extend{
    name = "qkinput",
}

function QkInput:onReaderReady()
    -- Plugins are WidgetContainers and are not shown, so there is no init();
    -- start the bridge when a document is ready (the file manager has no
    -- touch zones to drive anyway).
    if self.started then
        return
    end
    self.fifo = os.getenv("QK_TAP_FIFO")
    if not self.fifo then
        return
    end
    self.started = true
    self.fd = nil
    self.buffer = ""
    self.down = nil
    self.last = nil
    logger.info("qkinput: reading gestures from", self.fifo)
    self:schedule()
end

function QkInput:schedule()
    UIManager:scheduleIn(POLL_INTERVAL, function()
        local ok, err = pcall(self.poll, self)
        if not ok then
            logger.warn("qkinput: poll failed:", err)
        end
        self:schedule()
    end)
end

function QkInput:openFifo()
    if self.fd then
        return
    end
    local fd = ffi.C.open(self.fifo, O_RDONLY + O_NONBLOCK)
    if fd >= 0 then
        self.fd = fd
    end
end

function QkInput:nowMs()
    return time.boottime_or_realtime_coarse() * 1000
end

function QkInput:emit(ges)
    ges.time = time.s(self:nowMs() / 1000)
    logger.dbg("qkinput: emit", ges.ges, ges.direction or "", ges.pos and ges.pos.x, ges.pos and ges.pos.y)
    UIManager:broadcastEvent(Event:new("Gesture", ges))
end

function QkInput:emitTap(x, y)
    self:emit{
        ges = "tap",
        pos = Geom:new{ x = x, y = y, w = 0, h = 0 },
    }
end

function QkInput:emitSwipe(direction, x, y, distance)
    self:emit{
        ges = "swipe",
        direction = direction,
        pos = Geom:new{ x = x, y = y, w = 0, h = 0 },
        distance = distance or 0,
    }
end

function QkInput:handleCommand(line)
    local cmd = line:sub(1, 1)
    local x, y = line:match("^%a%s+(%-?%d+)%s+(%-?%d+)")
    x = tonumber(x)
    y = tonumber(y)

    if cmd == "t" and x and y then
        self.down = nil
        self:emitTap(x, y)
    elseif cmd == "d" and x and y then
        self.down = { x = x, y = y, at = self:nowMs() }
        self.last = { x = x, y = y }
    elseif cmd == "m" and x and y then
        if self.down then
            self.last = { x = x, y = y }
        end
    elseif cmd == "u" then
        if self.down and self.last then
            local dx = self.last.x - self.down.x
            local dy = self.last.y - self.down.y
            local adx, ady = math.abs(dx), math.abs(dy)
            if adx >= SWIPE_THRESHOLD or ady >= SWIPE_THRESHOLD then
                local direction
                if adx >= ady then
                    direction = dx >= 0 and "right" or "left"
                else
                    direction = dy >= 0 and "down" or "up"
                end
                self:emitSwipe(direction, self.down.x, self.down.y, math.max(adx, ady))
            elseif self:nowMs() - self.down.at <= MAX_TAP_MS then
                self:emitTap(self.down.x, self.down.y)
            end
        end
        self.down = nil
        self.last = nil
    end
end

function QkInput:poll()
    self:openFifo()
    if not self.fd then
        return
    end
    local buf = ffi.new("char[256]")
    for _ = 1, 4 do
        local n = tonumber(ffi.C.read(self.fd, buf, 256))
        if not n or n <= 0 then
            break
        end
        self.buffer = self.buffer .. ffi.string(buf, n)
    end
    while true do
        local newline = self.buffer:find("\n", 1, true)
        if not newline then
            break
        end
        local line = self.buffer:sub(1, newline - 1)
        self.buffer = self.buffer:sub(newline + 1)
        if line ~= "" then
            self:handleCommand(line)
        end
    end
end

return QkInput
