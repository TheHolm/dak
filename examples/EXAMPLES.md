# Examples

Small, complete configs you can copy next to `dak` (or point at with `dak -c`) and
adjust. Paths, device serials and commands are placeholders - change them to match your
system. Every example is a full config, runnable as-is once you have the hardware and any
helper tools it mentions.

- [`encoder-screen-brightness.json`](encoder-screen-brightness.json) - turn the third
  encoder to raise/lower the button-screen brightness in steps of 10. Declares a `step`
  variable and uses `bc` (which must be installed) to do the arithmetic:
  `$defaults.button_brightness ~= $(echo $defaults.button_brightness + $step | bc)`.
  `~=` clamps silently at 0 and 100, so winding past either end is a no-op rather than
  an error or a warning. The same config shows a live clock on button 1 and the current
  brightness on button 2, both using a per-button `refresh`; button 2 is a `text_value`
  showing `"$defaults.button_brightness%"` directly (no `echo` needed), re-expanded on
  every tick so it tracks the encoder's changes (the `%` is literal text, ending the
  reference name).

See the README's [Variables](../README.markdown#variables) section for the syntax these
examples use.
