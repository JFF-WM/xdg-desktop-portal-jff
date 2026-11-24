//! The life of one `ext-image-copy-capture-v1` session as a state machine: events from the
//! compositor in, what to do next out. No Wayland objects here, so the transitions can be tested
//! without a compositor (`wayland.rs` does the doing).
//!
//! The protocol in short: a session announces what a buffer must look like (`buffer_size`,
//! `shm_format`..., then `done`). A frame is a buffer attached and `capture`d, and ends in
//! `ready` or `failed`. jff only answers a frame when the source changed, so after `ready` the
//! next frame is asked for at once. A size change sends new constraints and fails the frame in
//! flight with `buffer_constraints`: allocate again and retry.

use crate::model::{pick_shm_format, valid_size, PixelFormat};

/// Why a frame failed (`ext_image_copy_capture_frame_v1.failure_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    Unknown,
    BufferConstraints,
    Stopped,
}

/// What the compositor said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    BufferSize(u32, u32),
    ShmFormat(u32),
    /// The constraints sent since the last `done` are complete.
    Done,
    FrameReady,
    FrameFailed(FailReason),
    /// The session ended (the window closed, the monitor went away).
    Stopped,
}

/// What to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Make a buffer (`reuse`: the one in use already fits, keep it), attach it to a new frame
    /// and capture.
    Capture {
        width: u32,
        height: u32,
        shm_format: u32,
        pixel: PixelFormat,
        reuse: bool,
    },
    /// The frame that just became ready holds a picture to read.
    Deliver,
    /// Nothing more will come: end the stream.
    Stop(String),
}

/// Failures in a row (other than a constraints change) before the capture is given up on.
const MAX_FAILURES: u32 = 5;

#[derive(Debug, Default)]
pub struct Machine {
    /// Constraints being received since the last `done`.
    pending_size: Option<(u32, u32)>,
    pending_formats: Vec<u32>,
    collecting: bool,
    /// The last complete constraints.
    current: Option<Complete>,
    /// What the buffer in use was made for.
    allocated: Option<(u32, u32, u32)>,
    in_flight: bool,
    /// A `done` arrived while a frame was in flight: that frame fails, and the retry can go.
    done_while_in_flight: bool,
    failures: u32,
    stopped: bool,
}

#[derive(Debug, Clone)]
struct Complete {
    width: u32,
    height: u32,
    formats: Vec<u32>,
}

impl Machine {
    pub fn new() -> Self {
        Self::default()
    }

    /// The size of the current constraints, once there are some.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.current.as_ref().map(|c| (c.width, c.height))
    }

    #[cfg(test)]
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    pub fn on_event(&mut self, event: Event) -> Vec<Action> {
        if self.stopped {
            return Vec::new();
        }
        match event {
            Event::BufferSize(w, h) => {
                self.begin_constraints();
                self.pending_size = Some((w, h));
                Vec::new()
            }
            Event::ShmFormat(f) => {
                self.begin_constraints();
                self.pending_formats.push(f);
                Vec::new()
            }
            Event::Done => self.constraints_done(),
            Event::FrameReady => {
                self.in_flight = false;
                self.failures = 0;
                let mut actions = vec![Action::Deliver];
                actions.extend(self.next_capture());
                actions
            }
            Event::FrameFailed(FailReason::Stopped) => self.stop("the source went away"),
            Event::FrameFailed(FailReason::BufferConstraints) => {
                self.in_flight = false;
                if self.done_while_in_flight {
                    self.done_while_in_flight = false;
                    self.next_capture().into_iter().collect()
                } else {
                    // The new constraints are still on their way: `done` starts the retry.
                    Vec::new()
                }
            }
            Event::FrameFailed(FailReason::Unknown) => {
                self.in_flight = false;
                self.failures += 1;
                if self.failures >= MAX_FAILURES {
                    return self.stop("the compositor kept failing frames");
                }
                self.next_capture().into_iter().collect()
            }
            Event::Stopped => self.stop("the source went away"),
        }
    }

    fn begin_constraints(&mut self) {
        if !self.collecting {
            self.collecting = true;
            self.pending_size = None;
            self.pending_formats.clear();
        }
    }

    fn constraints_done(&mut self) -> Vec<Action> {
        self.collecting = false;
        let Some((width, height)) = self.pending_size.take() else {
            return self.stop("the compositor sent no buffer size");
        };
        if !valid_size(width, height) {
            return self.stop(&format!("unusable source size {width}x{height}"));
        }
        let formats = std::mem::take(&mut self.pending_formats);
        if pick_shm_format(&formats).is_none() {
            return self.stop("the compositor offers no 8-bit shm format");
        }
        self.current = Some(Complete {
            width,
            height,
            formats,
        });
        if self.in_flight {
            self.done_while_in_flight = true;
            Vec::new()
        } else {
            self.next_capture().into_iter().collect()
        }
    }

    /// Ask for the next frame with the current constraints.
    fn next_capture(&mut self) -> Option<Action> {
        let current = self.current.as_ref()?;
        let (shm_format, pixel) = pick_shm_format(&current.formats)?;
        let want = (current.width, current.height, shm_format);
        let reuse = self.allocated == Some(want);
        self.allocated = Some(want);
        self.in_flight = true;
        Some(Action::Capture {
            width: current.width,
            height: current.height,
            shm_format,
            pixel,
            reuse,
        })
    }

    fn stop(&mut self, why: &str) -> Vec<Action> {
        self.stopped = true;
        self.in_flight = false;
        vec![Action::Stop(why.to_string())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SHM_ARGB8888, SHM_XRGB8888};

    fn constraints(m: &mut Machine, w: u32, h: u32) -> Vec<Action> {
        m.on_event(Event::BufferSize(w, h));
        m.on_event(Event::ShmFormat(SHM_ARGB8888));
        m.on_event(Event::ShmFormat(SHM_XRGB8888));
        m.on_event(Event::Done)
    }

    fn capture(w: u32, h: u32, reuse: bool) -> Action {
        Action::Capture {
            width: w,
            height: h,
            shm_format: SHM_ARGB8888,
            pixel: PixelFormat::Bgra,
            reuse,
        }
    }

    #[test]
    fn the_first_constraints_start_a_capture() {
        let mut m = Machine::new();
        assert_eq!(
            constraints(&mut m, 800, 600),
            vec![capture(800, 600, false)]
        );
        assert_eq!(m.size(), Some((800, 600)));
    }

    #[test]
    fn a_ready_frame_is_delivered_and_the_next_one_asked_for() {
        let mut m = Machine::new();
        constraints(&mut m, 800, 600);
        assert_eq!(
            m.on_event(Event::FrameReady),
            vec![Action::Deliver, capture(800, 600, true)]
        );
    }

    #[test]
    fn a_resize_reallocates_when_the_failure_comes_first() {
        let mut m = Machine::new();
        constraints(&mut m, 800, 600);
        // jff fails the frame in flight, then the new constraints arrive.
        assert_eq!(
            m.on_event(Event::FrameFailed(FailReason::BufferConstraints)),
            vec![]
        );
        assert_eq!(
            constraints(&mut m, 640, 480),
            vec![capture(640, 480, false)]
        );
    }

    #[test]
    fn a_resize_reallocates_when_done_comes_first() {
        let mut m = Machine::new();
        constraints(&mut m, 800, 600);
        // New constraints arrive while the frame is still in flight: nothing yet.
        assert_eq!(constraints(&mut m, 640, 480), vec![]);
        assert_eq!(
            m.on_event(Event::FrameFailed(FailReason::BufferConstraints)),
            vec![capture(640, 480, false)]
        );
    }

    #[test]
    fn constraints_that_did_not_change_keep_the_buffer() {
        let mut m = Machine::new();
        constraints(&mut m, 800, 600);
        m.on_event(Event::FrameFailed(FailReason::BufferConstraints));
        assert_eq!(constraints(&mut m, 800, 600), vec![capture(800, 600, true)]);
    }

    #[test]
    fn formats_of_an_older_batch_are_forgotten() {
        let mut m = Machine::new();
        constraints(&mut m, 800, 600);
        m.on_event(Event::FrameFailed(FailReason::BufferConstraints));
        // The new batch offers Xrgb8888 only.
        m.on_event(Event::BufferSize(800, 600));
        m.on_event(Event::ShmFormat(SHM_XRGB8888));
        assert_eq!(
            m.on_event(Event::Done),
            vec![Action::Capture {
                width: 800,
                height: 600,
                shm_format: SHM_XRGB8888,
                pixel: PixelFormat::Bgrx,
                reuse: false,
            }]
        );
    }

    #[test]
    fn a_closed_window_stops_the_session() {
        let mut m = Machine::new();
        constraints(&mut m, 800, 600);
        let actions = m.on_event(Event::Stopped);
        assert!(matches!(actions.as_slice(), [Action::Stop(_)]));
        assert!(m.is_stopped());
        assert_eq!(m.on_event(Event::FrameReady), vec![]);
    }

    #[test]
    fn a_frame_failing_as_stopped_stops_too() {
        let mut m = Machine::new();
        constraints(&mut m, 800, 600);
        let actions = m.on_event(Event::FrameFailed(FailReason::Stopped));
        assert!(matches!(actions.as_slice(), [Action::Stop(_)]));
    }

    #[test]
    fn unknown_failures_are_retried_a_few_times() {
        let mut m = Machine::new();
        constraints(&mut m, 800, 600);
        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(
                m.on_event(Event::FrameFailed(FailReason::Unknown)),
                vec![capture(800, 600, true)]
            );
        }
        let last = m.on_event(Event::FrameFailed(FailReason::Unknown));
        assert!(matches!(last.as_slice(), [Action::Stop(_)]));
    }

    #[test]
    fn a_success_resets_the_failure_count() {
        let mut m = Machine::new();
        constraints(&mut m, 800, 600);
        for _ in 0..MAX_FAILURES - 1 {
            m.on_event(Event::FrameFailed(FailReason::Unknown));
        }
        m.on_event(Event::FrameReady);
        assert_eq!(
            m.on_event(Event::FrameFailed(FailReason::Unknown)),
            vec![capture(800, 600, true)]
        );
    }

    #[test]
    fn bad_constraints_stop_the_session() {
        let mut m = Machine::new();
        // No size at all.
        let actions = m.on_event(Event::Done);
        assert!(matches!(actions.as_slice(), [Action::Stop(_)]));

        let mut m = Machine::new();
        m.on_event(Event::BufferSize(0, 100));
        m.on_event(Event::ShmFormat(SHM_ARGB8888));
        assert!(matches!(
            m.on_event(Event::Done).as_slice(),
            [Action::Stop(_)]
        ));

        // Only 10-bit formats.
        let mut m = Machine::new();
        m.on_event(Event::BufferSize(10, 10));
        m.on_event(Event::ShmFormat(0x30335258));
        assert!(matches!(
            m.on_event(Event::Done).as_slice(),
            [Action::Stop(_)]
        ));
    }
}
