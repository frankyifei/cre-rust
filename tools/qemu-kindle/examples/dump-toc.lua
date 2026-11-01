-- Example QK_SCRIPT for tools/qemu-kindle.
--
-- Print the TOC and jump to the last entry. This shows how to drive KOReader
-- through its own modules, without menu clicks.

local Event = require("ui/event")
local UIManager = require("ui/uimanager")
local logger = require("logger")

return function(ui)
    UIManager:scheduleIn(3, function()
        local toc = ui.document:getToc() or {}
        logger.info("debug: toc entries", #toc)
        for index, item in ipairs(toc) do
            logger.info("debug: toc", index, item.title, item.page)
        end
        if toc[1] then
            ui:handleEvent(Event:new("GotoXPointer", toc[1].xpointer, toc[1].xpointer))
            logger.info("debug: page", ui.document:getCurrentPage())
        end
    end)
end
