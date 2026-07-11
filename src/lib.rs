//! **MoshCatty** — pure Rust Mosh client, wire-compatible with stock `mosh-server`.
//!
//! Protocol layers (bottom → top):
//! - [`crypto`] — AES-128-OCB3 (RFC 7253) + mosh datagram framing
//! - [`fragment`] — instruction fragmentation (`network.cc` layout)
//! - [`pb`] — Transport / Host / User protobuf instruction codecs
//! - [`transport`] — State Synchronization Protocol (SSP)
//! - [`terminal`] — HostBytes paint helpers / strip_ansi
//! - [`framebuffer`] — client cell grid + Diff (mosh-go shape)
//! - [`ansi_apply`] — HostBytes ANSI → Framebuffer
//! - [`prediction`] — Predictor + DisplayPipeline (Confirm/Overlay)
//! - [`client`] — high-level UDP session
//!
//! Built for [Netcatty](https://github.com/binaricat/Netcatty) and standalone use.
//! No Cygwin, no terminfo database, no platform DLL bag.

#![deny(unsafe_code)]

pub mod ansi_apply;
pub mod client;
pub mod crypto;
pub mod display;
pub mod error;
pub mod framebuffer;
pub mod fragment;
pub mod pb;
pub mod prediction;
pub mod terminal;
pub mod transport;

pub use client::Client;
pub use crypto::Ocb;
pub use error::{Error, Result};
pub use framebuffer::Framebuffer;
pub use prediction::{DisplayPipeline, DisplayPreference, Predictor};
