//! **MoshCatty** — pure Rust Mosh client, wire-compatible with stock `mosh-server`.
//!
//! Protocol layers (bottom → top):
//! - [`crypto`] — AES-128-OCB3 (RFC 7253) + mosh datagram framing
//! - [`fragment`] — instruction fragmentation (`network.cc` layout)
//! - [`pb`] — Transport / Host / User protobuf instruction codecs
//! - [`transport`] — State Synchronization Protocol (SSP)
//! - [`terminal`] — apply HostBytes (Display::new_frame paint) for local output
//! - [`prediction`] — speculative local echo with underline (stock-like)
//! - [`client`] — high-level UDP session
//!
//! Built for [Netcatty](https://github.com/binaricat/Netcatty) and standalone use.
//! No Cygwin, no terminfo database, no platform DLL bag.

#![deny(unsafe_code)]

pub mod client;
pub mod crypto;
pub mod error;
pub mod fragment;
pub mod pb;
pub mod prediction;
pub mod terminal;
pub mod transport;

pub use client::Client;
pub use crypto::Ocb;
pub use error::{Error, Result};
pub use prediction::{DisplayPreference, LocalPredictor};
