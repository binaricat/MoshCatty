# Local prediction (speculative echo)

Status: **best-within-constraints** — stock overlay/keystroke/confirm semantics on a pure-Rust Diff paint path  
Related: [Netcatty #2121](https://github.com/binaricat/Netcatty/issues/2121)

## Architecture (must not change)

```text
HostBytes → apply_ansi (sticky carry) → host_fb → Confirm → Overlay → Diff(last_shown) → PTY
Keystroke → Predictor → same Diff path (paint only when show)
Frame watermarks → set_frames → Confirm (ack-only packets too)
Network timers → Notification → same Diff path (top row, restored on recovery)
```

**Loop order (live client):** `poll` → `on_host_bytes` (if paint) → `set_frames` (late_ack Confirm).  
Never Confirm late_ack against a stale `host_fb` before applying the same batch’s HostBytes.

Never dual-write raw predicted glyphs beside HostBytes.  
Never require terminfo / Cygwin / system mosh. Pure Rust standalone binary only.

## Alignment matrix (summary)

| Concern | Stock-aligned behavior |
|---------|------------------------|
| Paint path | Single Overlay→Diff; no dual-write |
| Epoch | start 1/0; hide until credited Correct; reset does not re-align conf |
| Confirm | late_ack Pending; blank pred always CorrectNoCredit; glitch repair only on Correct |
| Insert/BS | full-row maps; BS dual-unknown tail (`i+2`); overwrite space |
| Last-col print | known glyph + double become_tentative + wrap |
| Bottom CR | blank-predict full last row |
| CSI C/D | ±1 (params ignored) |
| ESC meta | Esc_Dispatch tentative only (no false glyph) |
| Host EL/ICH/DCH/ECH | Confirm final grid (not hard reset) |
| Host scroll / IL/DL / ED | wipe pending (`scroll_generation` / geometry CSI) |
| Always | forces show, not flagging |
| Adaptive | hold show while pending or cursor Pending |
| Experimental | show immediately; discard only a failed cell instead of its prediction band |
| Lifecycle | keep pending until late ACK / host validation; long-pending cells trigger display and underline |
| Network status | stock 250ms / 6.5s / 10s thresholds; notification shares the single Diff path |

## Env

| Variable | Values |
|----------|--------|
| `MOSH_PREDICTION_DISPLAY` | `adaptive` (default) / `always` / `never` / `experimental` |
| `MOSH_PREDICTION_OVERWRITE` | `yes`/`true`/`1` → overwrite instead of insert |

## Explicit non-goals (preserve MoshCatty advantages)

| Deferred | Why |
|----------|-----|
| System / Cygwin mosh-client / terminfo | Pure single-binary is the product |
| Full VTE emulator | HostBytes+Diff under node-pty is the fit |
| Bit-identical Diff vs stock `new_frame` | Different encoder; cell semantics matter |
| Title prefix chrome | Netcatty owns the session title |
| Scroll-history / up-down arrow prediction | Stock also defers / absent |

## Modules

- `framebuffer.rs` — cells + Diff + `scroll_generation`
- `ansi_apply.rs` — HostBytes → host_fb (+ scroll bumps generation)
- `prediction.rs` + `prediction_tests.rs` — Predictor + DisplayPipeline + prediction/network overlays + regression gates
- `mosh_client.rs` — host-before-ack loop wiring
