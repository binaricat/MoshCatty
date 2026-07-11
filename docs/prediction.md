# Local prediction (speculative echo)

Status: **mosh-go-shaped Framebuffer path** (default `MOSH_PREDICTION_DISPLAY=adaptive`)  
Related: [Netcatty #2121](https://github.com/binaricat/Netcatty/issues/2121)

## Architecture (aligned with mosh-go WASM `stateTracker`)

```text
HostBytes.hoststring
        │
        ▼
 apply_ansi → host_fb          // client cell grid
        │
 Predictor.confirm(host_fb)    // retire matching pending
        │
 display = host_fb.clone()
 Predictor.overlay(display)    // underline pending runes
        │
 Diff(last_shown, display)  →  single ANSI stream → PTY
 last_shown = display
```

On keystroke:

```text
Predictor.keystroke(keys)      // pending (rune,x,y); control → Reset
display = host_fb + Overlay
Diff(last_shown, display) → PTY
Client.send_keys(keys)         // still UDP to server
```

**Never** write predicted glyphs as a second raw stream beside HostBytes.

## References

| Source | Role |
|--------|------|
| [unixshells/mosh-go `predict.go`](https://github.com/unixshells/mosh-go/blob/main/predict.go) | Confirm / Overlay / Reset / ExpireStale API |
| mosh-go `framebuffer.go` | Cell grid + Diff / fullRedraw |
| mosh-go `cmd/mosh-wasm/state.go` | Composition order on host + keystroke |
| stock `terminaloverlay.cc` | Full epoch/cull engine (future fidelity) |
| stock `terminaldisplay.cc` | Why dual-write fails (`append_silent_move`) |

## Stock vs mosh-go vs MoshCatty

| Concern | Stock C++ | mosh-go | MoshCatty now |
|---------|-----------|---------|---------------|
| Model | Framebuffer | Framebuffer | Framebuffer |
| Predict | Overlay cells + BS row shift | pending (x,y) | pending (x,y) like go |
| Confirm | cull + epochs | Confirm(fb) | Confirm(fb) |
| Paint | new_frame(last, desired) | Diff | Diff |
| Control/BS | tentative / row shift | **Reset all** | **Reset all** (go) |
| Adaptive | 30/20 ms hysteresis | n/a | SRTT ≥ 20 ms |
| Underline | flagging hysteresis | always on pending | always on pending (go) |

## Env

| Value | Behavior |
|-------|----------|
| unset / `adaptive` | Default. Overlay when SRTT ≥ 20 ms; else HostBytes pass-through |
| `always` | Always overlay path |
| `never` | HostBytes pass-through only |

## Modules

- `src/framebuffer.rs` — cells + Diff  
- `src/ansi_apply.rs` — HostBytes → host_fb  
- `src/prediction.rs` — Predictor + DisplayPipeline  
- `src/bin/mosh_client.rs` — wires pipeline  
