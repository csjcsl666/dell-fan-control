-- Dell Fan Control: native background blur for its own window only.
--
-- hyprland/rules.lua (managed by illogical-impulse) disables blur for every
-- window. This file is required from custom/rules.lua, which hyprland.lua loads
-- after hyprland/rules.lua, and the last matching rule wins, so blur comes back
-- for this one app class and nothing else.
--
-- Nothing about placement is set: no float, size, position or pin. The window
-- tiles, floats, moves and resizes like any other. Opacity is fixed at 1.0 so
-- Hyprland's inactive_opacity does not stack on top of the app's own glass;
-- the app decides its translucency, and its opaque look stays fully opaque.
hl.window_rule({
    match = { class = "^dell-fan-control$" },
    no_blur = false,
    opacity = "1.0 override 1.0 override 1.0 override",
})
