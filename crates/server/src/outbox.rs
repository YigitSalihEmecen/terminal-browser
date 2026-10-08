//! Per-tab frame sender: diffs against what the client already has and applies back-pressure.
//!
//! The transport is ordered and reliable, so "what the client has" is simply the last frame we
//! sent. While `window` frames are unacknowledged we send nothing and keep only the *latest*
//! offered grid; when an ack frees a slot we diff `sent → latest` and send one message. Frames
//! in between are dropped, never queued.

use std::{collections::VecDeque, time::Instant};

use glyph_proto::{diff_runs, full_runs, Grid, Region, ServerMsg, TabId};

pub struct Outbox {
    tab: TabId,
    window: u64,
    seq: u64,
    acked: u64,
    sent: Option<Grid>,
    sent_regions: Vec<Region>,
    pending: Option<(Grid, Vec<Region>)>,
    inflight: VecDeque<(u64, Instant)>,
}

impl Outbox {
    /// `start_seq` keeps sequence numbers monotonic across tab switches, so a stale ack for an
    /// earlier incarnation of this tab can never acknowledge a newer frame.
    pub fn new(tab: TabId, window: u64, start_seq: u64) -> Self {
        Self {
            tab,
            window: window.max(1),
            seq: start_seq,
            acked: start_seq,
            sent: None,
            sent_regions: Vec::new(),
            pending: None,
            inflight: VecDeque::new(),
        }
    }

    /// The next message must be a FullFrame (client resized, switched tab, or asked to redraw).
    pub fn force_full(&mut self) {
        self.sent = None;
    }

    fn saturated(&self) -> bool {
        self.seq - self.acked >= self.window
    }

    /// A new rendered state. Returns what to send right now (possibly nothing).
    pub fn offer(&mut self, grid: Grid, regions: Vec<Region>) -> Vec<ServerMsg> {
        self.pending = Some((grid, regions));
        if self.saturated() {
            return Vec::new();
        }
        self.flush()
    }

    /// The client drew frame `seq`. Returns messages to send and the round-trip time in ms.
    pub fn ack(&mut self, seq: u64) -> (Vec<ServerMsg>, Option<f32>) {
        self.acked = self.acked.max(seq.min(self.seq));
        let mut rtt = None;
        while let Some(&(s, t)) = self.inflight.front() {
            if s > self.acked {
                break;
            }
            if s == seq {
                rtt = Some(t.elapsed().as_secs_f32() * 1000.0);
            }
            self.inflight.pop_front();
        }
        let out = if self.saturated() {
            Vec::new()
        } else {
            self.flush()
        };
        (out, rtt)
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    fn flush(&mut self) -> Vec<ServerMsg> {
        let Some((grid, regions)) = self.pending.take() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let frame = match &self.sent {
            Some(prev) if (prev.cols(), prev.rows()) == (grid.cols(), grid.rows()) => {
                let runs = diff_runs(prev, &grid);
                (!runs.is_empty()).then(|| ServerMsg::Diff {
                    tab: self.tab,
                    seq: self.seq + 1,
                    base: self.seq,
                    runs,
                })
            }
            _ => Some(ServerMsg::FullFrame {
                tab: self.tab,
                seq: self.seq + 1,
                cols: grid.cols(),
                rows: grid.rows(),
                runs: full_runs(&grid),
            }),
        };
        if let Some(f) = frame {
            self.seq += 1;
            self.inflight.push_back((self.seq, Instant::now()));
            out.push(f);
        }
        if regions != self.sent_regions {
            out.push(ServerMsg::Regions {
                tab: self.tab,
                seq: self.seq,
                regions: regions.clone(),
            });
            self.sent_regions = regions;
        }
        self.sent = Some(grid);
        out
    }
}

#[cfg(test)]
mod tests {
    use glyph_proto::{apply_runs, Style};

    use super::*;

    fn g(text: &str) -> Grid {
        let mut g = Grid::new(10, 2, Style::default());
        g.put_str(0, 0, text, Style::default(), 10);
        g
    }

    /// A client that applies messages the way the real one does.
    #[derive(Default)]
    struct Client {
        grid: Option<Grid>,
        last: u64,
    }
    impl Client {
        fn apply(&mut self, msgs: &[ServerMsg]) {
            for m in msgs {
                match m {
                    ServerMsg::FullFrame {
                        seq,
                        cols,
                        rows,
                        runs,
                        ..
                    } => {
                        let mut gr = Grid::new(*cols, *rows, Style::default());
                        apply_runs(&mut gr, runs);
                        self.grid = Some(gr);
                        self.last = *seq;
                    }
                    ServerMsg::Diff {
                        seq, base, runs, ..
                    } => {
                        assert_eq!(*base, self.last, "diff must chain onto the previous frame");
                        apply_runs(self.grid.as_mut().unwrap(), runs);
                        self.last = *seq;
                    }
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn first_frame_is_full_then_diffs() {
        let mut o = Outbox::new(1, 2, 0);
        let m = o.offer(g("abc"), vec![]);
        assert!(matches!(m[0], ServerMsg::FullFrame { seq: 1, .. }));
        let m = o.offer(g("abd"), vec![]);
        assert!(matches!(
            m[0],
            ServerMsg::Diff {
                seq: 2,
                base: 1,
                ..
            }
        ));
    }

    #[test]
    fn identical_state_sends_nothing() {
        let mut o = Outbox::new(1, 2, 0);
        o.offer(g("abc"), vec![]);
        assert!(o.offer(g("abc"), vec![]).is_empty());
    }

    #[test]
    fn saturated_window_drops_intermediate_frames_and_sends_latest_on_ack() {
        let mut o = Outbox::new(1, 2, 0);
        let mut c = Client::default();
        c.apply(&o.offer(g("a"), vec![])); // seq 1
        c.apply(&o.offer(g("ab"), vec![])); // seq 2, window now full
        assert!(o.offer(g("abc"), vec![]).is_empty());
        assert!(o.offer(g("abcd"), vec![]).is_empty());
        assert!(o.offer(g("abcde"), vec![]).is_empty());
        let (m, _) = o.ack(1);
        assert_eq!(m.len(), 1, "one coalesced diff, not three");
        c.apply(&m);
        assert_eq!(c.grid.as_ref().unwrap().dump_text(), g("abcde").dump_text());
        assert_eq!(c.last, 3);
    }

    #[test]
    fn force_full_resends_everything() {
        let mut o = Outbox::new(1, 2, 0);
        o.offer(g("a"), vec![]);
        o.force_full();
        let m = o.offer(g("a"), vec![]);
        assert!(matches!(m[0], ServerMsg::FullFrame { .. }));
    }

    #[test]
    fn resize_sends_full_frame() {
        let mut o = Outbox::new(1, 2, 0);
        o.offer(g("a"), vec![]);
        let m = o.offer(Grid::new(12, 3, Style::default()), vec![]);
        assert!(matches!(
            m[0],
            ServerMsg::FullFrame {
                cols: 12,
                rows: 3,
                ..
            }
        ));
    }

    #[test]
    fn client_always_converges_to_latest_offer() {
        // pseudo-random interleaving of offers and acks
        let mut o = Outbox::new(1, 2, 0);
        let mut c = Client::default();
        let texts = [
            "x",
            "xy",
            "",
            "zzzz",
            "日本",
            "ab日",
            "q",
            "qq",
            "日",
            "e\u{301}x",
        ];
        let mut state = 12345u32;
        let mut next = || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 16) as usize
        };
        let mut last_offer = String::new();
        let mut unacked: Vec<u64> = vec![];
        for _ in 0..500 {
            if next() % 3 != 0 {
                let t = texts[next() % texts.len()];
                last_offer = g(t).dump_text();
                let m = o.offer(g(t), vec![]);
                for x in &m {
                    if let ServerMsg::FullFrame { seq, .. } | ServerMsg::Diff { seq, .. } = x {
                        unacked.push(*seq);
                    }
                }
                c.apply(&m);
            } else if !unacked.is_empty() {
                let s = unacked.remove(0);
                let (m, _) = o.ack(s);
                for x in &m {
                    if let ServerMsg::FullFrame { seq, .. } | ServerMsg::Diff { seq, .. } = x {
                        unacked.push(*seq);
                    }
                }
                c.apply(&m);
            }
        }
        while !unacked.is_empty() {
            let s = unacked.remove(0);
            let (m, _) = o.ack(s);
            for x in &m {
                if let ServerMsg::FullFrame { seq, .. } | ServerMsg::Diff { seq, .. } = x {
                    unacked.push(*seq);
                }
            }
            c.apply(&m);
        }
        assert_eq!(c.grid.unwrap().dump_text(), last_offer);
    }
}
