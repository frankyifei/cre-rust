-- Example QK_SCRIPT for tools/qemu-kindle.
--
-- The file must return a function. The harness calls the function with the
-- reader UI after the book is ready. Use UIManager:scheduleIn() to wait for
-- KOReader to finish its own startup work.
--
-- Run it with:
--   QK_SCRIPT="$PWD/tools/qemu-kindle/examples/goto-page.lua" \
--       tools/qemu-kindle/run.sh fixtures/koreader-test-data/leaves.epub
--
-- The log output goes to ~/.cache/cre-qemu-kindle/reader.log.

local Event = require("ui/event")
local UIManager = require("ui/uimanager")
local logger = require("logger")

return function(ui)
    UIManager:scheduleIn(3, function()
        logger.info("debug: pages", ui.document:getPageCount())
        ui:handleEvent(Event:new("GotoPage", 20))
        logger.info("debug: page", ui.document:getCurrentPage())
    end)
end
