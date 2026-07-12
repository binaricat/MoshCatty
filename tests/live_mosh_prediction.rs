//! Live integration against a real mosh-server.
//!
//! Opt-in (never runs in default CI):
//!
//! ```bash
//! MOSH_LIVE_HOST=192.168.139.227 \
//! MOSH_LIVE_USER=root \
//! MOSH_LIVE_PASSWORD=... \
//! cargo test --test live_mosh_prediction -- --ignored --nocapture
//! ```
//!
//! Requires: `sshpass`, remote `mosh-server`, UDP reachability to the host.

use moshcatty::terminal::strip_ansi;
use moshcatty::{Client, DisplayPipeline, DisplayPreference};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

fn env_or_skip(key: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("skip: set {key} (and MOSH_LIVE_*) to run live mosh tests");
            String::new()
        }
    }
}

/// Start remote mosh-server via SSH; return (port, key).
fn start_remote_mosh_server(host: &str, user: &str, password: &str) -> (u16, String) {
    let remote_cmd = "mosh-server new -s -p 60000:60100 -- /bin/bash --noprofile --norc -c 'export PS1=\"$ \"; exec bash --noprofile --norc'";
    let output = Command::new("sshpass")
        .args([
            "-p",
            password,
            "ssh",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "ConnectTimeout=8",
            &format!("{user}@{host}"),
            remote_cmd,
        ])
        .output()
        .expect("sshpass/ssh must be available for live tests");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    // MOSH CONNECT <port> <key>
    for line in combined.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("MOSH CONNECT ") {
            let mut parts = rest.split_whitespace();
            let port: u16 = parts.next().expect("port").parse().expect("port number");
            let key = parts.next().expect("key").to_string();
            return (port, key);
        }
    }
    panic!("no MOSH CONNECT in remote output:\n{combined}");
}

fn poll_until<F>(client: &mut Client, deadline: Instant, mut pred: F) -> Vec<u8>
where
    F: FnMut(&[u8]) -> bool,
{
    let mut acc = Vec::new();
    while Instant::now() < deadline {
        let chunk = client.poll().expect("poll");
        if !chunk.is_empty() {
            acc.extend_from_slice(&chunk);
            if pred(&acc) {
                return acc;
            }
        }
        thread::sleep(Duration::from_millis(15));
    }
    acc
}

#[test]
#[ignore = "live mosh-server; set MOSH_LIVE_HOST/USER/PASSWORD"]
fn live_echo_and_prediction_pipeline_no_double_glyph() {
    let host = env_or_skip("MOSH_LIVE_HOST");
    if host.is_empty() {
        return;
    }
    let user = std::env::var("MOSH_LIVE_USER").unwrap_or_else(|_| "root".into());
    let password = env_or_skip("MOSH_LIVE_PASSWORD");
    if password.is_empty() {
        return;
    }

    let (port, key) = start_remote_mosh_server(&host, &user, &password);
    eprintln!("live: MOSH CONNECT {port} (key redacted)");

    let mut client = Client::dial(&host, port, &key).expect("dial mosh-server");
    client.resize(80, 24);

    // Drain initial paint (banner / prompt)
    let init = poll_until(&mut client, Instant::now() + Duration::from_secs(3), |_| {
        false
    });
    eprintln!(
        "live: initial paint plain={:?}",
        strip_ansi(&String::from_utf8_lossy(&init))
            .chars()
            .take(80)
            .collect::<String>()
    );

    let mut display = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    display.set_frames(
        client.sent_num(),
        client.acked_by_remote(),
        client.echo_ack(),
    );

    // Local prediction for "hello"
    let local = display.on_keystroke(b"hello");
    assert!(
        !local.is_empty(),
        "Always mode must paint local prediction Diff"
    );
    assert_eq!(display.predictor().pending_len(), 5);
    // last_shown should contain predicted glyphs
    let shown = display.last_shown().expect("last_shown");
    assert_eq!(shown.cell_at(0, 0).map(|c| c.ch), Some('h'));
    assert_eq!(shown.cell_at(4, 0).map(|c| c.ch), Some('o'));

    // Send to server
    client.send_keys(b"hello");
    // Force flush
    let _ = client.poll();

    // Wait for host echo
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut host_acc = Vec::new();
    while Instant::now() < deadline {
        display.set_frames(
            client.sent_num(),
            client.acked_by_remote(),
            client.echo_ack(),
        );
        let chunk = client.poll().expect("poll");
        if !chunk.is_empty() {
            host_acc.extend_from_slice(&chunk);
            let out = display.on_host_bytes(&chunk);
            let _ = out;
            let plain = strip_ansi(&String::from_utf8_lossy(&host_acc));
            if plain.contains('h') && plain.contains('o') {
                break;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }

    // After host echo + late_ack, pending should drain (or reduce)
    display.set_frames(
        client.sent_num(),
        client.acked_by_remote(),
        client.echo_ack(),
    );
    // One more confirm path via empty host? re-apply nothing — call confirm via on_host_bytes empty not useful.
    // Poll more
    for _ in 0..20 {
        display.set_frames(
            client.sent_num(),
            client.acked_by_remote(),
            client.echo_ack(),
        );
        let chunk = client.poll().expect("poll");
        if !chunk.is_empty() {
            let _ = display.on_host_bytes(&chunk);
        }
        if display.predictor().pending_len() == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(30));
    }

    eprintln!(
        "live: echo_ack={} sent={} early={} pending={} (pending may remain while late_ack < expiration_sent)",
        client.echo_ack(),
        client.sent_num(),
        client.acked_by_remote(),
        display.predictor().pending_len()
    );

    // Critical #2121 property: last_shown must not have doubled glyphs like "hheelllloo"
    let final_shown = display.last_shown().expect("last_shown");
    let mut row0: String = (0..20)
        .filter_map(|x| final_shown.cell_at(x, 0).map(|c| c.ch))
        .collect();
    row0 = row0.trim_end().to_string();
    eprintln!("live: row0 cells={row0:?}");
    assert!(
        !row0.contains("hh") && !row0.contains("ee"),
        "double-glyph pattern in row0={row0:?} (Netcatty #2121 regression)"
    );
    // Prefer seeing hello once
    assert!(
        row0.contains("hello")
            || row0.starts_with("hello")
            || host_acc.windows(5).any(|w| w == b"hello"),
        "expected hello in display or host stream; row0={row0:?} host={:?}",
        strip_ansi(&String::from_utf8_lossy(&host_acc))
    );

    // echo_ack should have advanced on a real server after keystrokes
    assert!(
        client.echo_ack() > 0 || display.predictor().pending_len() == 0,
        "real mosh-server should advance echo_ack after typed keys (got {})",
        client.echo_ack()
    );
}

#[test]
#[ignore = "live mosh-server; set MOSH_LIVE_HOST/USER/PASSWORD"]
fn live_client_survives_resize_and_more_keys() {
    let host = env_or_skip("MOSH_LIVE_HOST");
    if host.is_empty() {
        return;
    }
    let user = std::env::var("MOSH_LIVE_USER").unwrap_or_else(|_| "root".into());
    let password = env_or_skip("MOSH_LIVE_PASSWORD");
    if password.is_empty() {
        return;
    }

    let (port, key) = start_remote_mosh_server(&host, &user, &password);
    let mut client = Client::dial(&host, port, &key).expect("dial");
    client.resize(100, 30);
    let _ = poll_until(&mut client, Instant::now() + Duration::from_secs(2), |_| {
        false
    });
    client.send_keys(b"echo live-ok\n");
    let paint = poll_until(
        &mut client,
        Instant::now() + Duration::from_secs(5),
        |acc| {
            let s = strip_ansi(&String::from_utf8_lossy(acc));
            s.contains("live-ok")
        },
    );
    let plain = strip_ansi(&String::from_utf8_lossy(&paint));
    eprintln!(
        "live resize/cmd plain excerpt={:?}",
        plain.chars().take(120).collect::<String>()
    );
    assert!(
        plain.contains("live-ok"),
        "command output missing; plain={plain:?}"
    );
    assert!(!client.is_dead(), "client died: {:?}", client.dead_reason());
}
