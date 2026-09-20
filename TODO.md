# TODO / IDEAS

1. The FreeBSD build vendors its own copies of `mirajazz` and `async-hid` under
   `vendor/` (see `vendor/README.md` for why), pinned to specific upstream versions
   (`mirajazz` 0.16.0, `async-hid` 0.5.3) rather than tracking crates.io like every
   other platform's dependencies do. Nothing currently checks whether newer upstream
   releases of either crate exist. Add a CI job that periodically checks
   crates.io for newer `mirajazz`/`async-hid` versions than the ones vendored, so a
   security fix or bugfix upstream doesn't silently sit unnoticed for the FreeBSD
   build - see `vendor/README.md`'s "Updating" section for the manual re-vendoring
   steps such a check would need to prompt for.
2. Improve `-d scene` logging: add a single old->new scene-transition line
   (with cause: key press, timer, or command), timing info for
   `enter_scene`/`apply_scene_operations`, and gate scene-application
   skip/no-op warnings behind the `scene` debug subsystem instead of always
   printing them via `log.warn`.
3. Variables landed in v0.10.0 (declarations, `$name` substitution, the three
   assignment operators, and `$(command)` output capture - see [Variables](README.markdown#variables)).
   Still to come: per-scene variables (set only for the duration of one scene),
   arithmetic/expression right-hand sides (calculations are delegated to external tools
   like `bc` for now), explicit `str(...)`/`int(...)` type conversions, an `env.` scope
   for environment variables, and a per-scene "compute" section that runs commands to
   populate variables.
4. A built-in Lua (or similar) scripting language for advanced control - may be a bad
   idea: it's a much bigger surface (a whole embedded interpreter, its own error
   handling, a new config/script relationship to design) than anything else on this
   list, and might be better served by composing existing primitives (commands,
   variables once they exist, multiple actions per event) instead of adding a second
   configuration language alongside JSON.
5. Animated button images: a button's screen can already be redrawn continuously with
   a different image each frame at a solid 30fps, across all of a device's screens at
   once, with no config support needed for that at all - confirmed against real
   hardware, including a 3-hour zero-error run (see `NOTES.md` section 6 for the full
   findings, including where the real throughput ceiling is). What's missing is a way
   to *configure* one from `config.json`: today `setup` only ever pushes a single
   static frame (`"image"`/`"image_exec"`), with no way to say "cycle through these
   frames" or "call this command every N ms and redraw". Needs a new setup type (or an
   extension of the existing ones) plus a way to control the frame rate.
6. (Maybe - lower priority, and only relevant once #5 above exists) A sleep/low-power
   mode for animations specifically: redrawing a button continuously costs real,
   measurable CPU and HID bandwidth (see `NOTES.md` section 6) even though today's
   measured headroom is ample - it might be worth pausing an animation's redraws once
   its scene isn't the active one, rather than only when a config author remembers to
   stop it explicitly. There's no existing signal for "is anyone actually looking at
   this button" beyond scene switches and button presses, so this may not be worth the
   added complexity - noted as a "maybe", not a commitment.
7. Encoder LED *color* control from config (`mirajazz`'s `Device::set_led_colors`,
   distinct from the `encoder_brightness` default added in v0.9.0, which only
   controls overall LED brightness). Not implemented yet for two reasons: it can't be
   verified against real hardware (no Ajazz AKP03E/AKP03R unit with functioning
   encoder LEDs was available during development - `set_led_colors` and
   `set_led_brightness` both return success even on a unit with no LEDs wired up at
   all, so a successful call proves nothing), and the mapping between the LED-color
   array's index order and the physical/logical encoder numbering used elsewhere
   (`turn_cw`/`turn_ccw`/push references) is unconfirmed - a brute-force sweep setting
   one color-array index at a time across a wide range found no visible correlation
   before the sweep was cut short by the device repeatedly dropping off USB when
   hammered with this command. Needs a real unit with working encoder LEDs to
   properly map and add this safely.
