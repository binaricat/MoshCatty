//! Live integration against a real mosh-server.
//!
//! Opt-in (never runs in default CI):
//!
//! ```bash
//! MOSH_LIVE_HOST=192.168.139.227 \
//! MOSH_LIVE_USER=root \
//! MOSH_LIVE_SSH_KEY=~/.ssh/id_ed25519 \
//! cargo test --test live_mosh_prediction -- --ignored --nocapture
//! ```
//!
//! Password authentication is also available through `MOSH_LIVE_PASSWORD` and
//! requires `sshpass`. The remote host must have `mosh-server` and be reachable
//! over UDP.

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
fn start_remote_mosh_server(
    host: &str,
    user: &str,
    password: Option<&str>,
    ssh_key: Option<&str>,
) -> (u16, String) {
    let remote_cmd = "mosh-server new -s -p 60000:60100 -- /bin/bash --noprofile --norc -c 'export PS1=\"$ \"; exec bash --noprofile --norc'";
    let destination = format!("{user}@{host}");
    let mut command;
    if let Some(key) = ssh_key {
        command = Command::new("ssh");
        command.args(["-i", key, "-o", "BatchMode=yes"]);
    } else {
        command = Command::new("sshpass");
        command.args(["-p", password.expect("password or SSH key"), "ssh"]);
    }
    let output = command
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "ConnectTimeout=8",
            &destination,
            remote_cmd,
        ])
        .output()
        .expect("ssh/sshpass must be available for live tests");
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
#[ignore = "live mosh-server; set MOSH_LIVE_HOST and SSH credentials"]
fn live_echo_and_prediction_pipeline_no_double_glyph() {
    let host = env_or_skip("MOSH_LIVE_HOST");
    if host.is_empty() {
        return;
    }
    let user = std::env::var("MOSH_LIVE_USER").unwrap_or_else(|_| "root".into());
    let password = std::env::var("MOSH_LIVE_PASSWORD").ok();
    let ssh_key = std::env::var("MOSH_LIVE_SSH_KEY").ok();
    if password.is_none() && ssh_key.is_none() {
        eprintln!("skip: set MOSH_LIVE_PASSWORD or MOSH_LIVE_SSH_KEY");
        return;
    }

    let (port, key) =
        start_remote_mosh_server(&host, &user, password.as_deref(), ssh_key.as_deref());
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
    let mut last_remote_state_num = client.remote_state_num();
    let initial_frame = display.on_host_frame(client.remote_framebuffer());
    assert!(!initial_frame.is_empty(), "initial host frame must paint");
    display.set_frames(
        client.sent_num(),
        client.acked_by_remote(),
        client.echo_ack(),
    );

    // Local prediction for "hello"
    let local = display.on_keystroke(b"hello");
    assert!(display.predictor().pending_len() >= 5);
    eprintln!(
        "live: initial tentative prediction paint bytes={}",
        local.len()
    );

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
            let plain = strip_ansi(&String::from_utf8_lossy(&host_acc));
            if plain.contains('h') && plain.contains('o') {
                // Keep going until the reconstructed frame below has caught up.
            }
        }
        if client.remote_state_num() != last_remote_state_num {
            last_remote_state_num = client.remote_state_num();
            let _ = display.on_host_frame(client.remote_framebuffer());
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
            host_acc.extend_from_slice(&chunk);
        }
        if client.remote_state_num() != last_remote_state_num {
            last_remote_state_num = client.remote_state_num();
            let _ = display.on_host_frame(client.remote_framebuffer());
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

    // Critical #2121 property: the final reconstructed screen contains exactly
    // one command, regardless of which terminal row the shell prompt occupies.
    let final_shown = display.last_shown().expect("last_shown");
    let screen = (0..final_shown.rows)
        .map(|y| {
            (0..final_shown.cols)
                .filter_map(|x| final_shown.cell_at(x, y).map(|cell| cell.ch))
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    eprintln!("live: final screen={screen:?}");
    assert_eq!(
        screen.matches("hello").count(),
        1,
        "expected one hello on the final screen (Netcatty #2121); screen={screen:?} host={:?}",
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
#[ignore = "live mosh-server; set MOSH_LIVE_HOST and SSH credentials"]
fn live_client_survives_resize_and_more_keys() {
    let host = env_or_skip("MOSH_LIVE_HOST");
    if host.is_empty() {
        return;
    }
    let user = std::env::var("MOSH_LIVE_USER").unwrap_or_else(|_| "root".into());
    let password = std::env::var("MOSH_LIVE_PASSWORD").ok();
    let ssh_key = std::env::var("MOSH_LIVE_SSH_KEY").ok();
    if password.is_none() && ssh_key.is_none() {
        eprintln!("skip: set MOSH_LIVE_PASSWORD or MOSH_LIVE_SSH_KEY");
        return;
    }

    let (port, key) =
        start_remote_mosh_server(&host, &user, password.as_deref(), ssh_key.as_deref());
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
