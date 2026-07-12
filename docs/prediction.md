# Local prediction (speculative echo)

Status: **mosh-go core + stock fidelity extras** (default `adaptive`)  
Related: [Netcatty #2121](https://github.com/binaricat/Netcatty/issues/2121)

## Architecture

```text
HostBytes.hoststring
        │
        ▼
 apply_ansi → host_fb
        │
 Predictor.confirm(host_fb)
        │
 display = host_fb.clone()
 Predictor.overlay(display)   // underline iff flagging
        │
 Diff(last_shown, display) → single ANSI → PTY
```

Keystroke path: `Predictor.keystroke(keys, host_fb)` → same Diff path.  
Never dual-write raw predicted glyphs beside HostBytes.

## Alignment matrix

| Concern | Stock C++ | mosh-go | MoshCatty |
|---------|-----------|---------|-----------|
| Model | Framebuffer | Framebuffer | Framebuffer |
| Paint | new_frame single stream | Diff | Diff |
| Confirm | cull + epochs | Confirm(fb) | Confirm + frame Pending |
| Printable | insert + advance | pending (x,y) | pending + mid-line shift |
| Backspace | row shift / overwrite | **Reset all** | undo / shift pending / host-row insert BS |
| Left/right | CSI C/D | none | CSI C/D + SS3 |
| Other CSI/control | become_tentative | Reset | become_tentative (keep pending) |
| Tentative epochs | hide until proven | n/a | hide epoch > confirmed |
| Frame expiry | late_ack vs exp frame | 500ms wall | acked vs expiration_sent + 15s backup |
| Show adaptive | 30/20 ms + !active | n/a | 30/20 ms + pending hold |
| Underline | flagging 80/50 ms | always | flagging 80/50 ms |
| Glitch | 250ms show / 5s flag | 500ms expire | both (no latch after empty) |
| Wide glyph | tentative | treat as print | tentative (width≠1) |
| Last column | tentative | n/a | tentative |
| Bulk paste | reset >100 | always | reset >100 |

## Env

| Value | Behavior |
|-------|----------|
| `adaptive` (default) | Show when SRTT >30ms; underline when >80ms |
| `always` | Always show + underline |
| `never` | HostBytes pass-through only |

## Modules

- `framebuffer.rs` / `ansi_apply.rs` / `prediction.rs` / `mosh_client.rs`
