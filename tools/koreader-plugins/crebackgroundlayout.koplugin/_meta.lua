local _ = require("gettext")
return {
    fullname = _("Background layout"),
    description = _([[
Keeps the page total and the table of contents in sync while the engine lays the rest of the book out after a font, line spacing or margin change. Without it the footer and the TOC keep the numbers they had when the settings changed.]]),
}
