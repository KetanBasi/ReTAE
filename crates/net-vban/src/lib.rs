//! Network transport: VBAN packet codec, RX/TX threads, reorder window, jitter buffer,
//! VBAN-SERVICE/TEXT, mDNS discovery (PLAN.md §4, §5.2; CLAUDE.md laws 3 and 4).
//!
//! ```text
//!  UDP --> rx (recv 5 ms timeout, demux (src_ip, name), IP policy)
//!            --> packet (parse + decode to f32)
//!            --> reorder (4 slots by frame_counter) + jitter (conceal, play-out cap)
//!            --> rtrb ring == the strip's input port == the jitter buffer
//!                  --> audio-engine ClockDomainAdapter (drift resampler, S2)
//!
//!  bus output ring --> tx (1 ms tick) --> packet (encode) --> UDP (DSCP / qWAVE)
//! ```
#![deny(unsafe_code)]

pub mod discovery;
pub mod hook;
pub mod jitter;
#[cfg(feature = "opus")]
pub mod opus_codec;
pub mod packet;
pub mod qos;
pub mod reorder;
pub mod rx;
pub mod service;
pub mod stream;
pub mod timer;
pub mod tx;
