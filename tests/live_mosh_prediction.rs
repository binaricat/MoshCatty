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
use moshcatty::{Client, ConnectionStatus, DisplayPipeline, DisplayPreference, Ocb};
use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
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

struct RemoteServerGuard {
    host: String,
    user: String,
    password: Option<String>,
    ssh_key: Option<String>,
    pid: u32,
}

impl Drop for RemoteServerGuard {
    fn drop(&mut self) {
        let destination = format!("{}@{}", self.user, self.host);
        let remote_cleanup = format!(
            "pkill -TERM -P {} >/dev/null 2>&1 || true; kill {} >/dev/null 2>&1 || true",
            self.pid, self.pid
        );
        let mut command;
        if let Some(key) = self.ssh_key.as_deref() {
            command = Command::new("ssh");
            command.args(["-i", key, "-o", "BatchMode=yes"]);
        } else {
            command = Command::new("sshpass");
            command.args(["-p", self.password.as_deref().unwrap_or_default(), "ssh"]);
        }
        let _ = command
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "ConnectTimeout=3",
                &destination,
                &remote_cleanup,
            ])
            .output();
    }
}

/// Start remote mosh-server via SSH; return (port, key).
fn start_remote_mosh_server(
    host: &str,
    user: &str,
    password: Option<&str>,
    ssh_key: Option<&str>,
) -> (u16, String, RemoteServerGuard) {
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
    let detached_pid = combined
        .split("pid = ")
        .nth(1)
        .and_then(|tail| {
            tail.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or_else(|| panic!("no detached mosh-server pid in remote output:\n{combined}"));
    let server_guard = RemoteServerGuard {
        host: host.to_string(),
        user: user.to_string(),
        password: password.map(str::to_string),
        ssh_key: ssh_key.map(str::to_string),
        pid: detached_pid,
    };
    // MOSH CONNECT <port> <key>
    for line in combined.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("MOSH CONNECT ") {
            let mut parts = rest.split_whitespace();
            let port: u16 = parts.next().expect("port").parse().expect("port number");
            let key = parts.next().expect("key").to_string();
            return (port, key, server_guard);
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

fn framebuffer_text(client: &Client) -> String {
    let frame = client.remote_framebuffer();
    (0..frame.rows)
        .map(|y| {
            (0..frame.cols)
                .filter_map(|x| frame.cell_at(x, y).map(|cell| cell.ch))
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct UdpBlackholeProxy {
    port: u16,
    blackholed: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    server_packets: Arc<Mutex<Vec<Vec<u8>>>>,
    worker: Option<JoinHandle<()>>,
}

impl UdpBlackholeProxy {
    fn start(host: &str, server_port: u16) -> Self {
        let server = (host, server_port)
            .to_socket_addrs()
            .expect("resolve live server")
            .find(SocketAddr::is_ipv4)
            .expect("live blackhole test requires an IPv4 server address");
        let socket = UdpSocket::bind("0.0.0.0:0").expect("bind UDP proxy");
        let port = socket.local_addr().expect("proxy address").port();
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .expect("proxy read timeout");
        let blackholed = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let server_packets = Arc::new(Mutex::new(Vec::new()));
        let worker_blackholed = blackholed.clone();
        let worker_stopped = stopped.clone();
        let worker_server_packets = server_packets.clone();
        let worker = thread::spawn(move || {
            let mut client = None;
            let mut buf = [0u8; 65_535];
            while !worker_stopped.load(Ordering::SeqCst) {
                match socket.recv_from(&mut buf) {
                    Ok((len, source)) if source == server => {
                        worker_server_packets
                            .lock()
                            .expect("server packet capture")
                            .push(buf[..len].to_vec());
                        if !worker_blackholed.load(Ordering::SeqCst) {
                            if let Some(destination) = client {
                                let _ = socket.send_to(&buf[..len], destination);
                            }
                        }
                    }
                    Ok((len, source)) if source.ip().is_loopback() => {
                        client = Some(source);
                        if !worker_blackholed.load(Ordering::SeqCst) {
                            let _ = socket.send_to(&buf[..len], server);
                        }
                    }
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            blackholed,
            stopped,
            server_packets,
            worker: Some(worker),
        }
    }

    fn set_blackholed(&self, blackholed: bool) {
        self.blackholed.store(blackholed, Ordering::SeqCst);
    }

    fn clear_server_packets(&self) {
        self.server_packets
            .lock()
            .expect("server packet capture")
            .clear();
    }

    fn has_fragmented_server_instruction(&self, key: &str) -> bool {
        let ocb = Ocb::from_base64(key).expect("live MOSH_KEY");
        let mut fragments_by_instruction: HashMap<u64, HashSet<u16>> = HashMap::new();
        for packet in self
            .server_packets
            .lock()
            .expect("server packet capture")
            .iter()
        {
            let Some((_sequence, plaintext)) = ocb.open_datagram(packet) else {
                continue;
            };
            if plaintext.len() < 14 {
                continue;
            }
            let instruction_id = u64::from_be_bytes(plaintext[4..12].try_into().unwrap());
            let fragment_num = u16::from_be_bytes(plaintext[12..14].try_into().unwrap()) & 0x7fff;
            fragments_by_instruction
                .entry(instruction_id)
                .or_default()
                .insert(fragment_num);
        }
        fragments_by_instruction
            .values()
            .any(|fragment_nums| fragment_nums.len() > 1)
    }
}

impl Drop for UdpBlackholeProxy {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("UDP proxy worker");
        }
    }
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

    let (port, key, _server_guard) =
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
    assert!(
        client
            .graceful_shutdown(Duration::from_secs(10))
            .expect("graceful shutdown"),
        "stock server did not acknowledge prediction session shutdown"
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

    let (port, key, _server_guard) =
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
    assert!(
        client
            .graceful_shutdown(Duration::from_secs(10))
            .expect("graceful shutdown"),
        "stock server did not acknowledge resize session shutdown"
    );
}

#[test]
#[ignore = "live mosh-server; set MOSH_LIVE_HOST and SSH credentials"]
fn live_large_screen_update_reassembles_against_stock_server() {
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

    let (port, key, _server_guard) =
        start_remote_mosh_server(&host, &user, password.as_deref(), ssh_key.as_deref());
    let proxy = UdpBlackholeProxy::start(&host, port);
    let mut client = Client::dial_with_size("127.0.0.1", proxy.port, &key, 180, 70).expect("dial");
    client.resize(180, 70);
    let _ = poll_until(&mut client, Instant::now() + Duration::from_secs(2), |_| {
        false
    });
    proxy.clear_server_packets();

    let command = b"python3 -c 'import hashlib;[(print(\"MCLARGE%03d:\"%i+\"\".join(hashlib.sha256((\"%d:%d\"%(i,j)).encode()).hexdigest() for j in range(3))[:140])) for i in range(50)];print(\"MCLARGE_DONE\")'\n";
    client.send_keys(command);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut final_screen = String::new();
    while Instant::now() < deadline {
        client.poll().expect("poll");
        final_screen = framebuffer_text(&client);
        if final_screen.lines().any(|line| line == "MCLARGE_DONE") {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }

    assert!(
        final_screen.lines().any(|line| line == "MCLARGE_DONE"),
        "large output never reached its sentinel; screen={final_screen:?}"
    );
    let complete_bodies = final_screen
        .lines()
        .filter_map(|line| {
            let Some((prefix, body)) = line.split_once(':') else {
                return None;
            };
            (prefix.starts_with("MCLARGE")
                && prefix.len() == "MCLARGE000".len()
                && body.len() == 140
                && body.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then_some(body)
        })
        .collect::<HashSet<_>>();
    assert!(
        complete_bodies.len() >= 40,
        "expected at least 40 distinct intact large rows, got {}; screen={final_screen:?}",
        complete_bodies.len()
    );
    assert!(
        proxy.has_fragmented_server_instruction(&key),
        "stock server never emitted a multi-fragment instruction for the large update"
    );
    assert!(!client.is_dead(), "client died: {:?}", client.dead_reason());
    assert!(
        client
            .graceful_shutdown(Duration::from_secs(10))
            .expect("graceful shutdown"),
        "stock server did not acknowledge large-output session shutdown"
    );
}

#[test]
#[ignore = "live mosh-server; set MOSH_LIVE_HOST and SSH credentials"]
fn live_session_recovers_after_silent_udp_blackhole() {
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

    let (server_port, key, _server_guard) =
        start_remote_mosh_server(&host, &user, password.as_deref(), ssh_key.as_deref());
    let proxy = UdpBlackholeProxy::start(&host, server_port);
    let mut client = Client::dial_with_size("127.0.0.1", proxy.port, &key, 100, 30).expect("dial");
    client.resize(100, 30);
    let _ = poll_until(&mut client, Instant::now() + Duration::from_secs(3), |_| {
        false
    });
    client.send_keys(b"echo BEFORE_BLACKHOLE_OK\n");
    let before_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < before_deadline {
        client.poll().expect("poll before blackhole");
        if framebuffer_text(&client)
            .lines()
            .any(|line| line == "BEFORE_BLACKHOLE_OK")
        {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }
    assert!(
        framebuffer_text(&client)
            .lines()
            .any(|line| line == "BEFORE_BLACKHOLE_OK"),
        "session was not usable before blackhole"
    );

    proxy.set_blackholed(true);
    let outage_deadline = Instant::now() + Duration::from_secs(12);
    let mut saw_outage_notification = false;
    while Instant::now() < outage_deadline {
        client.poll().expect("poll during blackhole");
        if matches!(client.connection_status(), ConnectionStatus::LastContact(_)) {
            saw_outage_notification = true;
        }
        assert!(
            !client.is_dead(),
            "client died during a recoverable outage: {:?}",
            client.dead_reason()
        );
        thread::sleep(Duration::from_millis(15));
    }
    assert!(
        saw_outage_notification,
        "stock-compatible last-contact notification never appeared"
    );
    proxy.set_blackholed(false);

    client.send_keys(b"echo AFTER_BLACKHOLE_OK\n");
    let recovery_deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < recovery_deadline {
        client.poll().expect("poll after blackhole");
        if framebuffer_text(&client)
            .lines()
            .any(|line| line == "AFTER_BLACKHOLE_OK")
        {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }
    let recovered_screen = framebuffer_text(&client);
    assert!(
        recovered_screen
            .lines()
            .any(|line| line == "AFTER_BLACKHOLE_OK"),
        "same session did not recover after blackhole; screen={recovered_screen:?}"
    );
    assert_eq!(
        client.connection_status(),
        ConnectionStatus::Online,
        "network notification did not clear after recovery"
    );
    assert!(
        client
            .graceful_shutdown(Duration::from_secs(10))
            .expect("graceful shutdown"),
        "stock server did not acknowledge recovered session shutdown"
    );
}
