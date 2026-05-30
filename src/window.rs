use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZcWindowLimits {
    pub max_descriptors: u64,
    pub max_bytes: u64,
    pub max_sequence_span: u64,
}

impl ZcWindowLimits {
    pub const fn unbounded() -> Self {
        Self {
            max_descriptors: u64::MAX,
            max_bytes: u64::MAX,
            max_sequence_span: u64::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZcWindowReservation {
    pub lane_id: u32,
    pub sequence_start: u64,
    pub sequence_end: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZcWindowWatermark {
    pub lane_id: u32,
    pub next_sequence: u64,
    pub committed_sequence: u64,
    pub released_sequence: u64,
    pub in_flight_descriptors: u64,
    pub in_flight_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZcWindowError {
    EmptyReservation,
    DescriptorCreditExhausted,
    ByteCreditExhausted,
    SequenceCreditExhausted,
    SequenceOverflow,
    CommitBeyondIssued,
    ReleaseBeyondIssued,
}

impl fmt::Display for ZcWindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyReservation => write!(f, "window reservation must have non-zero span"),
            Self::DescriptorCreditExhausted => write!(f, "descriptor window credit exhausted"),
            Self::ByteCreditExhausted => write!(f, "byte window credit exhausted"),
            Self::SequenceCreditExhausted => write!(f, "sequence window credit exhausted"),
            Self::SequenceOverflow => write!(f, "window sequence overflow"),
            Self::CommitBeyondIssued => write!(f, "window commit is beyond issued sequence"),
            Self::ReleaseBeyondIssued => write!(f, "window release is beyond issued sequence"),
        }
    }
}

impl std::error::Error for ZcWindowError {}

#[derive(Clone, Debug)]
pub struct ZcLaneWindow {
    lane_id: u32,
    limits: ZcWindowLimits,
    next_sequence: u64,
    committed_sequence: u64,
    released_sequence: u64,
    in_flight_descriptors: u64,
    in_flight_bytes: u64,
}

impl ZcLaneWindow {
    pub fn new(lane_id: u32, limits: ZcWindowLimits) -> Self {
        Self {
            lane_id,
            limits,
            next_sequence: 0,
            committed_sequence: 0,
            released_sequence: 0,
            in_flight_descriptors: 0,
            in_flight_bytes: 0,
        }
    }

    pub fn reserve(
        &mut self,
        sequence_span: u64,
        bytes: u64,
    ) -> Result<ZcWindowReservation, ZcWindowError> {
        if sequence_span == 0 {
            return Err(ZcWindowError::EmptyReservation);
        }
        if self.in_flight_descriptors >= self.limits.max_descriptors {
            return Err(ZcWindowError::DescriptorCreditExhausted);
        }
        let next_sequence = self
            .next_sequence
            .checked_add(sequence_span)
            .ok_or(ZcWindowError::SequenceOverflow)?;
        let in_flight_bytes = self
            .in_flight_bytes
            .checked_add(bytes)
            .ok_or(ZcWindowError::ByteCreditExhausted)?;
        if in_flight_bytes > self.limits.max_bytes {
            return Err(ZcWindowError::ByteCreditExhausted);
        }
        let active_span = next_sequence
            .checked_sub(self.released_sequence)
            .ok_or(ZcWindowError::SequenceOverflow)?;
        if active_span > self.limits.max_sequence_span {
            return Err(ZcWindowError::SequenceCreditExhausted);
        }
        let reservation = ZcWindowReservation {
            lane_id: self.lane_id,
            sequence_start: self.next_sequence,
            sequence_end: next_sequence,
            bytes,
        };
        self.next_sequence = next_sequence;
        self.in_flight_descriptors += 1;
        self.in_flight_bytes = in_flight_bytes;
        Ok(reservation)
    }

    pub fn commit_until(&mut self, sequence: u64) -> Result<(), ZcWindowError> {
        if sequence > self.next_sequence {
            return Err(ZcWindowError::CommitBeyondIssued);
        }
        self.committed_sequence = self.committed_sequence.max(sequence);
        Ok(())
    }

    pub fn release(&mut self, reservation: ZcWindowReservation) -> Result<(), ZcWindowError> {
        if reservation.sequence_end > self.next_sequence {
            return Err(ZcWindowError::ReleaseBeyondIssued);
        }
        self.released_sequence = self.released_sequence.max(reservation.sequence_end);
        self.in_flight_descriptors = self.in_flight_descriptors.saturating_sub(1);
        self.in_flight_bytes = self.in_flight_bytes.saturating_sub(reservation.bytes);
        Ok(())
    }

    pub fn watermark(&self) -> ZcWindowWatermark {
        ZcWindowWatermark {
            lane_id: self.lane_id,
            next_sequence: self.next_sequence,
            committed_sequence: self.committed_sequence,
            released_sequence: self.released_sequence,
            in_flight_descriptors: self.in_flight_descriptors,
            in_flight_bytes: self.in_flight_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_window_tracks_reserve_commit_release() {
        let mut window = ZcLaneWindow::new(
            7,
            ZcWindowLimits {
                max_descriptors: 2,
                max_bytes: 1024,
                max_sequence_span: 16,
            },
        );
        let first = window.reserve(4, 512).unwrap();
        assert_eq!(first.sequence_start, 0);
        assert_eq!(first.sequence_end, 4);
        window.commit_until(4).unwrap();
        assert_eq!(window.watermark().committed_sequence, 4);
        let second = window.reserve(4, 512).unwrap();
        assert_eq!(
            window.reserve(1, 1).unwrap_err(),
            ZcWindowError::DescriptorCreditExhausted
        );
        window.release(first).unwrap();
        assert_eq!(window.watermark().released_sequence, 4);
        window.release(second).unwrap();
        assert_eq!(window.watermark().in_flight_bytes, 0);
    }
}
