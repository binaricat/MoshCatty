# Local prediction (speculative echo)

Status: **not production-ready** (default `MOSH_PREDICTION_DISPLAY=never`)  
Related: [Netcatty #2121](https://github.com/binaricat/Netcatty/issues/2121)

## Stock mosh (C++)

Sources: `src/frontend/terminaloverlay.cc`, `stmclient.cc`, `terminaldisplay.cc`.

```text
server framebuffer
       │
       ▼
 overlays.apply(predictions)   // cell overlay, optional underline (flagging)
       │
       ▼
 Display::new_frame(last_drawn, desired)  → single ANSI stream → real TTY
```

| Concern | Mechanism |
|---------|-----------|
| Model | Full cell `Framebuffer` of host state |
| Predict | `new_user_byte`: printable insert, BS row-shift, left/right arrow |
| Confirm | `cull`: match host cells + frame acks / epochs |
| Underline | Separate **flagging** hysteresis (≈80/50 ms), not always-on |
| Adaptive | **srtt_trigger** hysteresis (≈30 ms on / 20 ms off; off only when idle) |
| Safety | One writer: predictions never dual-write the live PTY |

`Display::new_frame` uses `append_silent_move`: if the encoder's cursor is already on the target cell, it emits a **relative** glyph write (no CUP). That is correct only when the real terminal matches the encoder's model.

## mosh-go

Source: [unixshells/mosh-go `predict.go`](https://github.com/unixshells/mosh-go/blob/main/predict.go).

| Concern | Mechanism |
|---------|-----------|
| Model | Client `Framebuffer` |
| Predict | Pending `(rune, x, y)` for printable only |
| Confirm | `Confirm(fb)` cell rune match; diverge → `Reset()` |
| Overlay | Set cell + `Attr.Under`; move cursor |
| Control / BS | **Reset all** (simpler than stock row-shift) |
| Adaptive | Not ported (always-on style + 500 ms expire) |

Still **grid + overlay** — never raw echo beside HostBytes.

## MoshCatty today

- Host path: stream `HostBytes.hoststring` (server-side `Display::new_frame` output).
- No client cell grid.
- Experimental dual-write predictor (opt-in): write `\e[4m`+char to PTY, then later stream HostBytes; clear a counter on paint.

### Why dual-write breaks

1. User types `l` → client writes `l`, real cursor advances.
2. Server echoes `l`; `new_frame` often assumes cursor still at the unechoed cell and emits relative `l`.
3. Relative host write lands at **cursor+1** → `$ ll` for one key, `$ lls` for `ls`.

Clearing `outstanding` is not confirm: it does not unpaint, and unrelated HostBytes drop the counter while ghosts remain.

## Correct next step (minimum)

Adopt **mosh-go shape** before enabling adaptive by default:

1. Parse/apply HostBytes into a client cell grid + cursor.
2. `Confirm` + `Overlay` pending predictions on that grid.
3. Single paint path: `diff(last_shown, host⊕overlay)` → PTY.
4. Then port stock adaptive/flagging hysteresis.

Defer full stock epoch/glitch/row-shift until the grid path works.

## Env

| Value | Behavior |
|-------|----------|
| unset / `never` | **Default.** HostBytes only (safe). |
| `always` / `adaptive` | Experimental dual-write; **may garble**. |
