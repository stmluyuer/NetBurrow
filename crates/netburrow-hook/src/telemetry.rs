//! Bounded metadata only. Formatting and disk writes belong to the worker, not game calls.
use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};

const FLOW_LIMIT: usize = 64;
const CHANNEL_LIMIT: usize = 32;
const EVENT_LIMIT: usize = 64;

#[derive(Clone, Default)]
pub struct Flow {
    pub sent: u64,
    pub sent_bytes: u64,
    pub rejected: u64,
    pub received: u64,
    pub received_bytes: u64,
    pub consumed: u64,
    pub consumed_bytes: u64,
    pub dropped: u64,
    pub stale_received: u64,
    pub cleared_in: u64,
    pub cleared_out: u64,
    pub last_send: Option<Instant>,
    pub last_receive: Option<Instant>,
    pub last_read: Option<Instant>,
}

#[derive(Clone, Default)]
struct Poll {
    available: u64,
    available_empty: u64,
    reads: u64,
    reads_empty: u64,
}

#[derive(Clone)]
struct Event {
    at: Instant,
    member: u64,
    kind: &'static str,
    channel: Option<i32>,
    incoming: usize,
    outgoing: usize,
}

#[derive(Clone, Default)]
pub struct Telemetry {
    // Relay temporary member number; never Steam IDs or epoch values.
    flows: BTreeMap<(u64, i32, u8), Flow>,
    polls: BTreeMap<i32, Poll>,
    events: VecDeque<Event>,
    omitted_updates: u64,
    omitted_events: u64,
    pub session_calls: u64,
    pub session_inactive: u64,
    pub session_failed: u64,
}

impl Telemetry {
    pub fn flow(&mut self, member: u64, channel: i32, kind: u8) -> Option<&mut Flow> {
        let key = (member, channel, kind);
        if self.flows.len() >= FLOW_LIMIT && !self.flows.contains_key(&key) {
            self.omitted_updates += 1;
            return None;
        }
        Some(self.flows.entry(key).or_default())
    }

    pub fn poll(&mut self, channel: i32, read: bool, found: bool) {
        if self.polls.len() >= CHANNEL_LIMIT && !self.polls.contains_key(&channel) {
            self.omitted_updates += 1;
            return;
        }
        let p = self.polls.entry(channel).or_default();
        if read {
            p.reads += 1;
            p.reads_empty += u64::from(!found);
        } else {
            p.available += 1;
            p.available_empty += u64::from(!found);
        }
    }

    pub fn event(
        &mut self,
        member: u64,
        kind: &'static str,
        channel: Option<i32>,
        incoming: usize,
        outgoing: usize,
    ) {
        if self.events.len() == EVENT_LIMIT {
            self.events.pop_front();
            self.omitted_events += 1;
        }
        self.events.push_back(Event {
            at: Instant::now(),
            member,
            kind,
            channel,
            incoming,
            outgoing,
        });
    }

    pub fn take_snapshot(&mut self) -> Self {
        let snapshot = self.clone();
        self.events.clear();
        snapshot
    }

    pub fn lines(&self) -> Vec<String> {
        let now = Instant::now();
        let age = |at: Option<Instant>| {
            at.map_or_else(
                || "never".to_owned(),
                |at| now.saturating_duration_since(at).as_millis().to_string(),
            )
        };
        let mut lines = vec![format!(
            "summary session_calls={} session_inactive={} session_failed={} omitted_updates={} omitted_events={} counters=cumulative",
            self.session_calls,
            self.session_inactive,
            self.session_failed,
            self.omitted_updates,
            self.omitted_events
        )];
        for (&(member, channel, kind), f) in &self.flows {
            lines.push(format!("flow member={member} channel={channel} kind={kind} sent={} sent_bytes={} rejected={} received={} received_bytes={} consumed={} consumed_bytes={} dropped={} stale_received={} cleared_in={} cleared_out={} send_age_ms={} receive_age_ms={} read_age_ms={}", f.sent, f.sent_bytes, f.rejected, f.received, f.received_bytes, f.consumed, f.consumed_bytes, f.dropped, f.stale_received, f.cleared_in, f.cleared_out, age(f.last_send), age(f.last_receive), age(f.last_read)));
        }
        for (channel, p) in &self.polls {
            lines.push(format!("poll channel={channel} available_calls={} available_empty={} read_calls={} read_empty={}", p.available, p.available_empty, p.reads, p.reads_empty));
        }
        for e in &self.events {
            lines.push(format!(
                "event member={} reason={} channel={} cleared_in={} cleared_out={} age_ms={}",
                e.member,
                e.kind,
                e.channel.map_or_else(|| "all".into(), |c| c.to_string()),
                e.incoming,
                e.outgoing,
                age(Some(e.at))
            ));
        }
        lines
    }
}
