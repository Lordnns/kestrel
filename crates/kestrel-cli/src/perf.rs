//! Performance measurement for `kestrel run --stats`: how long the host
//! took between the frames a program showed, toward the real-time goal of
//! up to 120 frames a second (`PLAN.md` section 1).

use kestrel_hle::display::{Frame, Screen};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Passes frames on and records the host time each was shown.
pub struct Timed {
    pub times: Rc<RefCell<Vec<Instant>>>,
    pub inner: Option<Box<dyn Screen>>,
}

impl Screen for Timed {
    fn present(&mut self, frame: &Frame) {
        self.times.borrow_mut().push(Instant::now());
        if let Some(inner) = &mut self.inner {
            inner.present(frame);
        }
    }
}

/// Frame times between consecutive frames.
#[derive(Debug, PartialEq)]
pub struct Summary {
    /// Frames shown (one more than the frame times).
    pub frames: usize,
    pub average: Duration,
    /// The 99th percentile frame time: 1% of frames took at least this
    /// long. Its rate is the "1% low" (kestrel's choice of definition;
    /// others average the slowest 1%).
    pub low_1: Duration,
    pub worst: Duration,
}

impl Summary {
    /// `None` with fewer than two frames.
    pub fn of(times: &[Instant]) -> Option<Self> {
        let mut gaps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();
        Self::of_gaps(&mut gaps, times.len())
    }

    fn of_gaps(gaps: &mut [Duration], frames: usize) -> Option<Self> {
        if gaps.is_empty() {
            return None;
        }
        gaps.sort();
        let total: Duration = gaps.iter().sum();
        // The smallest frame time at or above which 1% of them lie.
        let k = gaps.len() - gaps.len().div_ceil(100);
        Some(Summary {
            frames,
            average: total / gaps.len() as u32,
            low_1: gaps[k],
            worst: gaps[gaps.len() - 1],
        })
    }

    pub fn describe(&self) -> String {
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let fps = |d: Duration| 1.0 / d.as_secs_f64().max(1e-9);
        format!(
            "{} frames; frame time average {:.2} ms ({:.1} fps), 1% low {:.1} fps ({:.2} ms), worst {:.2} ms",
            self.frames,
            ms(self.average),
            fps(self.average),
            fps(self.low_1),
            ms(self.low_1),
            ms(self.worst)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn frame_times_are_summarised() {
        // 199 frames of 8 ms and one of 50 ms: average (199 * 8 + 50) /
        // 200 = 8.21 ms; the slowest 1% are the 2 slowest of 200, so the
        // 1% low is the second slowest, 8 ms; the worst is 50 ms.
        let mut gaps = vec![ms(8); 199];
        gaps.insert(57, ms(50));
        let s = Summary::of_gaps(&mut gaps, 201).unwrap();
        assert_eq!(s.frames, 201);
        assert_eq!(s.average, Duration::from_micros(8210));
        assert_eq!((s.low_1, s.worst), (ms(8), ms(50)));
        // With fewer than 100 frame times the slowest is the 1% low.
        let s = Summary::of_gaps(&mut [ms(5), ms(9), ms(7)], 4).unwrap();
        assert_eq!((s.low_1, s.worst, s.average), (ms(9), ms(9), ms(7)));
        assert_eq!(Summary::of(&[Instant::now()]), None);
    }
}
