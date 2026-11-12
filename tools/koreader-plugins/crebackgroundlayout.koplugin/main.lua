-- Keep KOReader's page numbers in sync with the engine's background relayout.
--
-- Changing the font, line spacing or margins lays out the chunk under the
-- reader immediately, then the rest of the book on a worker thread; the page
-- table is rebuilt as chunks are installed. Page numbers are a prefix sum of
-- chunk pages, so the total and every page number move during that pass.
--
-- KOReader caches both numbers: ReaderFooter keeps `self.pages` until a
-- "TocReset"/"DocumentRerendered", and ReaderToc keeps the page numbers it read
-- at layout time until "DocumentRerendered". crengine produces that event by
-- reloading the document from its background rerendering subprocess; our engine
-- lays the chunks out in-process, so nobody tells the reader.
--
-- This plugin does it without patching KOReader:
--   * on every "PageUpdate"/"PosUpdate", re-read the page total and send
--     "TocReset" when it moved, which is what makes ReaderFooter re-read it;
--   * while `document:hasPendingRerendering()` says the worker still has
--     chunks, poll once a second (reading the page count is also what installs
--     the chunks that are ready, and it applies a deferred settings change, so
--     the pass advances while the reader is idle), and send one
--     "DocumentRerendered" when it is over, so ReaderToc rebuilds with the
--     final page numbers.
--
-- Everything here goes through events and the document's public API, so
-- KOReader can keep following upstream without a local patch.

local Event = require("ui/event")
local UIManager = require("ui/uimanager")
local WidgetContainer = require("ui/widget/container/widgetcontainer")
local logger = require("logger")

local BackgroundLayout = WidgetContainer:extend{
    name = "crebackgroundlayout",
    is_doc_only = true,
}

local POLL_SECONDS = 1

-- `CREBG_LOG=1` writes one line per decision to the reader log; the plugin is
-- otherwise silent, and a "the total did not change" report needs to show
-- whether the pass was noticed at all.
local function log(...)
    if os.getenv("CREBG_LOG") then
        logger.info("crebackgroundlayout:", ...)
    end
end

function BackgroundLayout:init()
    -- Last page total the reader has been told about. Nil until the document
    -- has one, so the first update is not treated as a change.
    self.pages = nil
    self.polling = false
    -- True while a background pass we noticed is still running; the finishing
    -- "DocumentRerendered" is sent only for a pass we actually saw start.
    self.pass_seen = false
    -- A single stable function: UIManager:unschedule matches by identity.
    self.poll = function() self:onPoll() end
end

-- The helpers below bail out on documents that are not ours (PDF, and any
-- engine without the background pass): `hasPendingRerendering` is only
-- registered by cre-lua. It is reached through `_document` because a plugin
-- cannot add a method to the CreDocument wrapper without patching it, and the
-- wrapper only forwards the calls it knows about.
local function engine_document(self)
    local doc = self.ui and self.ui.document
    local inner = doc and doc._document
    if inner and inner.hasPendingRerendering then
        return doc, inner
    end
    return nil
end

--- Reads the page total, tells the reader modules when it moved, and returns
--- whether the engine still has chunks to lay out.
---
--- Reading the page count is not a passive query: the engine applies a deferred
--- settings change and installs the chunk layouts the worker has finished, on
--- the reader's thread. That is why the pending flag is trustworthy right after
--- this call, and why polling alone advances the pass.
function BackgroundLayout:observe()
    local doc, inner = engine_document(self)
    if not doc then
        return nil
    end
    local pages = doc:getPageCount()
    if pages ~= self.pages then
        log("page total", self.pages, "->", pages)
        self.pages = pages
        self:publish(pages)
    end
    return inner:hasPendingRerendering()
end

--- Tells the reader modules the total moved.
---
--- `TocReset` is the event `ReaderFooter:onTocReset()` listens for, and it also
--- rebuilds the footer's progress markers, so it is the documented path. The
--- footer itself lives on `ReaderView`, though, and an event that only walks the
--- window stack does not always reach it (reported from a device: the current
--- page followed the pass, the total did not). Writing the footer's own field
--- and repainting is exactly what its handler would do, so do both.
function BackgroundLayout:publish(pages)
    local footer = self.ui.view and self.ui.view.footer
    local before = footer and footer.pages
    self.ui:handleEvent(Event:new("TocReset"))
    local after_event = footer and footer.pages
    if footer and after_event ~= pages then
        footer.pages = pages
        if footer.onUpdateFooter then
            footer:onUpdateFooter()
        end
    end
    log(
        "publish", pages,
        "footer before", tostring(before),
        "after event", tostring(after_event),
        "now", tostring(footer and footer.pages)
    )
end

--- (Re)starts watching a document. Safe to call more than once.
function BackgroundLayout:restart(why)
    self.pages = nil
    self.pass_seen = false
    log("restart after", why or "event")
    self:watch()
end

function BackgroundLayout:watch()
    local pending = self:observe()
    if pending == nil then
        self.polling = false
        return
    end
    if not pending then
        -- A pass we saw start has ended: the numbers are final now.
        if self.pass_seen then
            self:finish()
        end
        self.polling = false
        return
    end
    self.pass_seen = true
    if self.polling then
        return -- already polling
    end
    self.polling = true
    log("pass started")
    UIManager:scheduleIn(POLL_SECONDS, self.poll)
end

function BackgroundLayout:onPoll()
    local doc = engine_document(self)
    if not doc then
        self.polling = false
        return
    end
    if self:observe() then
        UIManager:scheduleIn(POLL_SECONDS, self.poll)
        return
    end
    self.polling = false
    self:finish()
end

--- The pass is over: page numbers, the total and the ToC are final.
function BackgroundLayout:finish()
    if not self.pass_seen then
        return
    end
    self.pass_seen = false
    self.polling = false
    UIManager:unschedule(self.poll)
    log("pass finished, total", self.pages)
    -- The same event a full rerendering sends: ReaderToc drops the page numbers
    -- it read while the book was still being laid out (so the ToC is exact the
    -- next time it is opened), ReaderFooter refreshes, and ReaderUI saves the
    -- final page count with the book.
    self.ui:handleEvent(Event:new("DocumentRerendered"))
end

function BackgroundLayout:onReaderReady()
    -- Also fires when the reader reloads the document (a style change closes and
    -- reopens it), so this must re-arm rather than only run once.
    self:restart("reader ready")
end

function BackgroundLayout:onCloseDocument()
    self.pass_seen = false
    self.polling = false
    self.pages = nil
    UIManager:unschedule(self.poll)
end

function BackgroundLayout:onDocumentRerendered()
    -- A settings change went through (or our own pass just ended): the engine
    -- may still have chunks to lay out.
    self:restart("DocumentRerendered")
end

function BackgroundLayout:onDocumentPartiallyRerendered()
    -- Not sent while `isRerenderingDelayed` is false, but harmless and it is a
    -- precise "a pass just started" signal if that ever changes.
    self:watch()
end

function BackgroundLayout:onPageUpdate()
    self:refresh()
end

function BackgroundLayout:onPosUpdate()
    self:refresh()
end

--- Re-read the page total on a page turn (or a position change).
function BackgroundLayout:refresh()
    local pending = self:observe()
    if pending == nil then
        return
    end
    if pending and not self.polling then
        -- A pass started without an event we saw (a reloaded document, a
        -- settings change KOReader applied silently): pick it up here.
        self:watch()
    end
end

return BackgroundLayout
