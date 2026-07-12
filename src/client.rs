//! High-level mosh client session (UDP + SSP + HostBytes paint).
//!
//! Stock mosh-server puts `Display::new_frame` output in HostBytes.hoststring
//! (ANSI-like paint). Writing that stream to a real PTY/xterm is correct for
//! Netcatty's node-pty sandwich and eliminates the Cygwin terminfo path.

use std::net::{ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use crate::crypto::Ocb;
use crate::error::{Error, Result};
use crate::pb::UserInstruction;
use crate::terminal::TerminalView;
use crate::transport::Transport;

const TICK_INTERVAL: Duration = Duration::from_millis(8);
const MAX_PAYLOAD: usize = 16384;
/// Exit when no authenticated datagram has arrived for this long (stock-like).
const NETWORK_TIMEOUT: Duration = Duration::from_secs(60);

/// A connected mosh client session.
pub struct Client {
    socket: UdpSocket,
    transport: Transport,
    terminal: TerminalView,
    actions: Vec<UserInstruction>,
    acked_action_count: usize,
    last_acked: u64,
    sent_action_counts: Vec<(u64, usize)>,
    dirty: bool,
    last_tick: Instant,
    /// Set when UDP peer is gone / hard error — CLI should exit.
    dead: bool,
    dead_reason: Option<String>,
}

impl Client {
    /// Connect to a mosh-server endpoint.
    pub fn dial(host: &str, port: u16, key: &str) -> Result<Self> {
        let ocb = Ocb::from_base64(key)?;
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(Error::Io)?
            .next()
            .ok_or_else(|| Error::Other(format!("could not resolve {host}:{port}")))?;

        let socket = UdpSocket::bind(if addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })?;
        socket.connect(addr)?;
        socket.set_read_timeout(Some(Duration::from_millis(20)))?;
        socket.set_write_timeout(Some(Duration::from_secs(5)))?;

        let mut transport = Transport::new_client(ocb);
        transport.force_next_send();
        let mut client = Self {
            socket,
            transport,
            terminal: TerminalView::new(80, 24),
            actions: Vec::new(),
            acked_action_count: 0,
            last_acked: 0,
            sent_action_counts: Vec::new(),
            dirty: false,
            last_tick: Instant::now(),
            dead: false,
            dead_reason: None,
        };
        client.flush_ticks()?;
        Ok(client)
    }

    pub fn send_keys(&mut self, keys: &[u8]) {
        if keys.is_empty() || self.dead {
            return;
        }
        self.actions.push(UserInstruction::keystroke(keys.to_vec()));
        self.dirty = true;
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if self.dead {
            return;
        }
        self.actions
            .push(UserInstruction::resize(cols as i32, rows as i32));
        self.dirty = true;
    }

    /// True after network death / timeout — caller should exit the process.
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    pub fn dead_reason(&self) -> Option<&str> {
        self.dead_reason.as_deref()
    }

    /// Smoothed RTT from the transport, if any sample is available.
    pub fn srtt(&self) -> Option<std::time::Duration> {
        self.transport.srtt()
    }

    /// SSP state numbers for stock-style prediction frame expiry.
    pub fn sent_num(&self) -> u64 {
        self.transport.sent_num()
    }

    pub fn acked_by_remote(&self) -> u64 {
        self.transport.acked_by_remote()
    }

    /// Stock prediction uses ~SRTT/2 clamped to 20–250ms as `send_interval`.
    pub fn send_interval(&self) -> Option<std::time::Duration> {
        self.srtt().map(|d| {
            let half_ms = (d.as_millis() as u64) / 2;
            let ms = half_ms.clamp(20, 250);
            std::time::Duration::from_millis(ms)
        })
    }

    /// Poll network + flush pending ticks. Returns newly painted local bytes.
    pub fn poll(&mut self) -> Result<Vec<u8>> {
        if self.dead {
            return Ok(Vec::new());
        }

        let mut paint = Vec::new();
        let mut buf = [0u8; MAX_PAYLOAD + 64];
        loop {
            match self.socket.recv(&mut buf) {
                Ok(n) => {
                    if let Some(diff) = self.transport.recv(&buf[..n]) {
                        let chunk = self.terminal.apply_host_diff(&diff);
                        paint.extend_from_slice(&chunk);
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionRefused
                        || e.kind() == std::io::ErrorKind::ConnectionReset =>
                {
                    self.mark_dead("network connection reset");
                    break;
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }

        if self.transport.last_recv().elapsed() > NETWORK_TIMEOUT {
            self.mark_dead("mosh did not hear from the server (network timeout)");
            return Ok(paint);
        }

        if self.last_tick.elapsed() >= TICK_INTERVAL {
            self.flush_ticks()?;
            self.last_tick = Instant::now();
        }

        Ok(paint)
    }

    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut acc = Vec::new();
        while Instant::now() < deadline && !self.dead {
            let chunk = self.poll()?;
            if !chunk.is_empty() {
                acc.extend_from_slice(&chunk);
                let more = self.poll()?;
                acc.extend_from_slice(&more);
                return Ok(acc);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(acc)
    }

    pub fn terminal(&self) -> &TerminalView {
        &self.terminal
    }

    fn mark_dead(&mut self, reason: &str) {
        self.dead = true;
        self.dead_reason = Some(reason.to_string());
    }

    fn flush_ticks(&mut self) -> Result<()> {
        if self.dead {
            return Ok(());
        }
        if self.dirty && self.transport.acked_by_remote() >= self.transport.sent_num() {
            self.dirty = false;
            self.process_acks();
            let new_actions = self.actions[self.acked_action_count..].to_vec();
            if !new_actions.is_empty() {
                let payload = UserInstruction::encode_message(&new_actions);
                self.transport.set_pending(payload);
                let next_num = self.transport.sent_num() + 1;
                self.sent_action_counts.push((next_num, self.actions.len()));
            }
        }

        for dg in self.transport.tick() {
            match self.socket.send(&dg) {
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionRefused
                        || e.kind() == std::io::ErrorKind::ConnectionReset =>
                {
                    self.mark_dead("network connection refused");
                    return Ok(());
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }
        Ok(())
    }

    fn process_acks(&mut self) {
        let acked = self.transport.acked_by_remote();
        if acked > self.last_acked {
            self.last_acked = acked;
            if acked >= self.transport.sent_num() {
                if let Some(&(_, count)) = self.sent_action_counts.iter().find(|(n, _)| *n == acked)
                {
                    if count > self.acked_action_count {
                        self.acked_action_count = count;
                    }
                }
                self.sent_action_counts.clear();
                // Drop fully-acked keystroke history (long-session bound).
                if self.acked_action_count > 0 && self.acked_action_count <= self.actions.len() {
                    self.actions.drain(..self.acked_action_count);
                    self.acked_action_count = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Ocb;
    use crate::pb::HostInstruction;
    use crate::transport::Transport;
    use std::net::UdpSocket;
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn spawn_echo_server(marker: &'static str) -> (u16, String, thread::JoinHandle<()>) {
        let mut key = [0u8; 16];
        for (i, b) in key.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }
        let key_b64 = {
            use base64::Engine;
            let s = base64::engine::general_purpose::STANDARD.encode(key);
            s.trim_end_matches('=').to_string()
        };

        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        sock.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();

        let done = Arc::new(Mutex::new(false));
        let done2 = done.clone();

        let handle = thread::spawn(move || {
            let ocb = Ocb::new(&key).unwrap();
            let mut transport = Transport::new_server(ocb);
            let mut client_addr = None;
            let mut sent_banner = false;
            let mut buf = [0u8; 4096];

            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(8) {
                if *done2.lock().unwrap() {
                    break;
                }
                match sock.recv_from(&mut buf) {
                    Ok((n, addr)) => {
                        client_addr = Some(addr);
                        if let Some(diff) = transport.recv(&buf[..n]) {
                            if !diff.is_empty() {
                                let host = HostInstruction::encode_message(&[HostInstruction {
                                    hoststring: format!(
                                        "\x1b[H\x1b[2J$ echo {marker}\r\n{marker}\r\n$ "
                                    )
                                    .into_bytes(),
                                    width: 0,
                                    height: 0,
                                    echo_ack_num: -1,
                                }]);
                                transport.set_pending(host);
                            }
                        }
                        if !sent_banner {
                            let host = HostInstruction::encode_message(&[HostInstruction {
                                hoststring: b"\x1b[H\x1b[2J$ ".to_vec(),
                                width: 0,
                                height: 0,
                                echo_ack_num: -1,
                            }]);
                            transport.set_pending(host);
                            sent_banner = true;
                        }
                    }
                    Err(_) => {}
                }
                if let Some(addr) = client_addr {
                    for dg in transport.tick() {
                        let _ = sock.send_to(&dg, addr);
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }
            *done2.lock().unwrap() = true;
        });

        thread::sleep(Duration::from_millis(20));
        (port, key_b64, handle)
    }

    #[test]
    fn client_dial_recv_and_command_against_fake_server() {
        let marker = "NETCATTY_MOSH_UNIT_OK";
        let (port, key, handle) = spawn_echo_server(marker);

        let mut client = Client::dial("127.0.0.1", port, &key).expect("dial");
        let mut saw_prompt = false;
        for _ in 0..100 {
            let out = client.poll().unwrap();
            if !out.is_empty() {
                saw_prompt = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_prompt, "expected initial banner paint");

        client.send_keys(format!("echo {marker}\n").as_bytes());
        let mut all = String::new();
        for _ in 0..200 {
            let out = client.poll().unwrap();
            if !out.is_empty() {
                all.push_str(&String::from_utf8_lossy(&out));
            }
            if crate::terminal::strip_ansi(&all).contains(marker) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = handle.join();
        panic!("marker not found in output: {all:?}");
    }
}
