//! Bounded replay state for one reliable connection direction.
//!
//! The stored bytes are already encoded inner-message frames, including their
//! four-byte length prefix. This module deliberately does not inspect them.

use std::{
    collections::VecDeque,
    io::{self, ErrorKind},
};

const MAX_PENDING_FRAMES: usize = 4096;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

/// Send replay and receive ordering state for one reliable stream.
#[derive(Debug, Default)]
pub struct Window {
    pending: VecDeque<(u64, Vec<u8>)>,
    pending_bytes: usize,
    last_sent: u64,
    received_through: u64,
}

impl Window {
    /// Retains a fully encoded frame and assigns its next outgoing sequence.
    pub fn retain(&mut self, body: Vec<u8>) -> io::Result<u64> {
        let sequence = self
            .last_sent
            .checked_add(1)
            .ok_or_else(|| invalid("outgoing sequence overflow"))?;
        let pending_bytes = self
            .pending_bytes
            .checked_add(body.len())
            .ok_or_else(|| capacity("resume window byte capacity exceeded"))?;
        if self.pending.len() >= MAX_PENDING_FRAMES {
            return Err(capacity("resume window frame capacity exceeded"));
        }
        if pending_bytes > MAX_PENDING_BYTES {
            return Err(capacity("resume window byte capacity exceeded"));
        }

        self.pending.push_back((sequence, body));
        self.pending_bytes = pending_bytes;
        self.last_sent = sequence;
        Ok(sequence)
    }

    /// Applies a cumulative acknowledgement to retained outgoing frames.
    pub fn acknowledge(&mut self, through: u64) -> io::Result<()> {
        if through > self.last_sent {
            return Err(invalid("acknowledgement exceeds sent sequence"));
        }

        while self
            .pending
            .front()
            .is_some_and(|(sequence, _)| *sequence <= through)
        {
            let (_, body) = self.pending.pop_front().expect("front was checked");
            self.pending_bytes -= body.len();
        }
        Ok(())
    }

    /// Returns a replay snapshot in original send order.
    pub fn pending(&self) -> Vec<(u64, Vec<u8>)> {
        self.pending.iter().cloned().collect()
    }

    /// Classifies a received sequence without changing receive state.
    ///
    /// `Ok(true)` is exactly the next expected sequence. `Ok(false)` is a
    /// duplicate. Gaps and zero are protocol errors.
    pub fn classify(&self, sequence: u64) -> io::Result<bool> {
        if sequence == 0 {
            return Err(invalid("zero receive sequence"));
        }
        if sequence <= self.received_through {
            return Ok(false);
        }
        let expected = self
            .received_through
            .checked_add(1)
            .ok_or_else(|| invalid("incoming sequence overflow"))?;
        if sequence == expected {
            Ok(true)
        } else {
            Err(invalid("received sequence gap"))
        }
    }

    /// Records the next incoming sequence; duplicate sequences are no-ops.
    pub fn received(&mut self, sequence: u64) -> io::Result<()> {
        if self.classify(sequence)? {
            self.received_through = sequence;
        }
        Ok(())
    }

    /// The greatest consecutive incoming sequence received so far.
    pub fn received_through(&self) -> u64 {
        self.received_through
    }

    /// Number of unacknowledged outgoing frames.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Number of bytes retained for unacknowledged outgoing frames.
    pub fn pending_bytes(&self) -> usize {
        self.pending_bytes
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

fn capacity(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::WouldBlock, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retain_and_cumulative_acknowledgement_preserve_order() {
        let mut window = Window::default();
        assert_eq!(window.retain(vec![0, 0, 0, 1, 1]).unwrap(), 1);
        assert_eq!(window.retain(vec![0, 0, 0, 1, 2]).unwrap(), 2);
        assert_eq!(window.pending_bytes(), 10);

        window.acknowledge(1).unwrap();
        window.acknowledge(1).unwrap();
        assert_eq!(window.pending(), vec![(2, vec![0, 0, 0, 1, 2])]);
        assert!(window.acknowledge(3).is_err());
        assert_eq!(window.pending_len(), 1);
    }

    #[test]
    fn duplicate_receive_is_a_noop_and_gaps_are_rejected() {
        let mut window = Window::default();
        assert!(window.classify(1).unwrap());
        window.received(1).unwrap();
        assert!(!window.classify(1).unwrap());
        window.received(1).unwrap();
        assert_eq!(window.received_through(), 1);

        assert!(window.classify(0).is_err());
        assert!(window.classify(3).is_err());
        assert!(window.received(3).is_err());
        assert_eq!(window.received_through(), 1);
        window.received(2).unwrap();
        assert_eq!(window.received_through(), 2);
    }

    #[test]
    fn byte_capacity_rejects_without_mutating_window() {
        let mut window = Window::default();
        window.retain(vec![0; MAX_PENDING_BYTES]).unwrap();
        assert!(window.retain(vec![1]).is_err());
        assert_eq!(window.pending_len(), 1);
        assert_eq!(window.pending_bytes(), MAX_PENDING_BYTES);

        window.acknowledge(1).unwrap();
        assert_eq!(window.retain(vec![2]).unwrap(), 2);
    }

}
