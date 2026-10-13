local _ = require("gettext")

return {
    fullname = _("turbosearch reader probe"),
    description = _([[Verifies the plugin against the real reader. Inert unless TURBOSEARCH_PROBE=1.]]),
}
