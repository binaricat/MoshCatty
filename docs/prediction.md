# Local prediction (speculative echo)

Status: **stock-aligned Diff path** (default `adaptive`)  
Related: [Netcatty #2121](https://github.com/binaricat/Netcatty/issues/2121)

## Architecture (must not change)

```text
HostBytes → apply_ansi (sticky carry) → host_fb → Confirm → Overlay → Diff(last_shown) → PTY
Keystroke → Predictor → same Diff path (paint only when show)
```

Never dual-write raw predicted glyphs beside HostBytes.  
Never require terminfo / Cygwin / system mosh.

## Alignment matrix

| Concern | Stock C++ | mosh-go | MoshCatty |
|---------|-----------|---------|-----------|
| Model | Framebuffer | Framebuffer | Framebuffer |
| Paint | new_frame | Diff | Diff |
| Confirm | cull + epochs | Confirm(fb) | Confirm + frame Pending |
| Printable | insert shift | pending | pending + host-row insert + mid shift (any epoch) |
| Backspace | row shift / overwrite space | Reset | undo / pending shift / host-row BS / overwrite space |
| Overwrite mode | CLI flag | n/a | `MOSH_PREDICTION_OVERWRITE` (print + BS) |
| L/R arrows | CSI C/D (+params as +1) | none | CSI n C/D (count, clamp) + SS3 + cursor_exp until ack |
| CR | tentative + row | n/a | tentative + row (no scroll) |
| Tentative epochs | hide until proven | n/a | hide epoch > confirmed |
| Frame Pending | late_ack | n/a | acked vs expiration_sent |
| Adaptive | predict always; apply gated | n/a | background predict when cold; Overlay gated |
| Adaptive show | send_interval 30/20 | n/a | send_interval≈SRTT/2 ∈[20,250] |
| Flagging | 80/50 ms | always under | 80/50 ms |
| Glitch | 250ms / 5s + 150ms repair | 500ms expire | true-oldest age + no empty latch |
| Row change | prove anew | n/a | become_tentative |
| Renditions | match left + Correct row cascade | n/a | inherit left Attr; Correct copies host Attr to rest of row |
| CorrectNoCredit | blank/noop/unknown | n/a | space/noop/unknown |
| unknown cells | underline only | n/a | underline only (no glyph replace) |
| Last column | place + tentative + wrap | n/a | same |
| Wide / combining | wcwidth≠1 tentative | n/a | width 0/2 → tentative |
| Cursor only | ConditionalCursorMove | n/a | cursor_exp_sent + confirm (empty pending) |
| Host model | full VT | minimal | CUP/SGR/EL/ED/ECH/ICH/DCH/IL/DL/scroll/wrap + sticky ESC/UTF-8 |
| Structural host ops | cull | n/a | ICH/DCH/IL/DL/ED/EL/ECH (incl. split CSI via carry) + bottom scroll → reset pending |
| SS3 non L/R | tentative | n/a | consume ESC O X fully (no printable pollution) |
| Bulk paste | reset >100 | always | reset >100 |
| Keystroke UTF-8 | parser stream | n/a | sticky multi-byte carry across keystroke chunks |

## Env

| Variable | Values |
|----------|--------|
| `MOSH_PREDICTION_DISPLAY` | `adaptive` (default) / `always` / `never` |
| `MOSH_PREDICTION_OVERWRITE` | `yes`/`true`/`1` → overwrite instead of insert |

## Explicitly NOT implemented (protect Netcatty advantages)

| Rejected | Why |
|----------|-----|
| System / Cygwin mosh-client | Breaks pure single-binary Windows path |
| terminfo | Same |
| Full VTE-scale emulator | HostBytes+Diff sandwich is the product fit under node-pty |
| Forced alt-screen / exclusive TTY | Conflicts with `MOSH_NO_TERM_INIT` + xterm.js primary buffer |
| Scroll history prediction | Stock deferred; high garble risk |
| Up/down arrow prediction | Stock does not either |
| Notification / title chrome | Not Diff-path echo; Netcatty has own UI |
| Dual-write PTY echo | #2121 class bug |
| `local_frame_late_acked` dual watermark | Only if SSP exposes a clean late watermark — do not invent wire changes |
| Password/no-echo special-case heuristics | Stock relies on prove-anew; avoid re-echo secrets |

## Modules

- `framebuffer.rs` — cells + Diff (+ wide rune width)
- `ansi_apply.rs` — HostBytes → host_fb (sticky incomplete ESC/UTF-8, DECAWM-style wrap)
- `prediction.rs` + `prediction_tests.rs` — Predictor + DisplayPipeline
- `mosh_client.rs` — frames + send_interval wiring
