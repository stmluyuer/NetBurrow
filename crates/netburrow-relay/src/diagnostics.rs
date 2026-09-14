//! Server diagnostics go to stderr so the service manager owns retention.
//! Only fixed event names, local connection numbers and aggregate counters belong here.

use std::{
    fmt,
    io::{self, Write},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) fn record(level: &str, event: &str, details: fmt::Arguments<'_>) {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    // A closed stderr must not panic or bring down the relay.
    let _ = writeln!(
        io::stderr().lock(),
        "unix_ms={timestamp} level={level} event={event} {details}"
    );
}

#[derive(Default)]
pub(crate) struct Stats {
    pub accepted: AtomicU64,
    pub handshake_rejected: AtomicU64,
    pub capacity_rejected: AtomicU64,
    pub joined: AtomicU64,
    pub disconnected: AtomicU64,
    pub tcp_data_received: AtomicU64,
    pub tcp_data_written: AtomicU64,
    pub udp_data_received: AtomicU64,
    pub udp_data_sent: AtomicU64,
    pub udp_invalid: AtomicU64,
    pub udp_bind_rejected: AtomicU64,
    pub protocol_rejected: AtomicU64,
    pub queue_failed: AtomicU64,
    pub io_failed: AtomicU64,
}

impl Stats {
    pub fn summary(
        &self,
        online: usize,
        game_bound: usize,
        udp_bound: usize,
        queued_bytes: usize,
    ) -> String {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        format!(
            "online={online} game_bound={game_bound} udp_bound={udp_bound} queued_bytes={queued_bytes} accepted={} joined={} disconnected={} handshake_rejected={} capacity_rejected={} tcp_data_received={} tcp_data_written={} udp_data_received={} udp_data_sent={} udp_invalid={} udp_bind_rejected={} protocol_rejected={} queue_failed={} io_failed={}",
            get(&self.accepted),
            get(&self.joined),
            get(&self.disconnected),
            get(&self.handshake_rejected),
            get(&self.capacity_rejected),
            get(&self.tcp_data_received),
            get(&self.tcp_data_written),
            get(&self.udp_data_received),
            get(&self.udp_data_sent),
            get(&self.udp_invalid),
            get(&self.udp_bind_rejected),
            get(&self.protocol_rejected),
            get(&self.queue_failed),
            get(&self.io_failed),
        )
    }
}

pub(crate) fn increment(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}
