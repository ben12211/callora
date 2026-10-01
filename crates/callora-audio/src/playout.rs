//! Paced, cancellable playout of μ-law audio to a telephone leg.
//!
//! Audio is sent in 20 ms frames, only a few frames ahead of real time. That small lead is
//! what makes barge-in instant: on cancel there are at most `lead` frames in flight, and a
//! `Clear` tells the telephony side to drop even those. Pacing also means the runtime knows
//! when a reply has actually been *heard* (the `Idle` event), which is when hangups and
//! transfers may happen.
//!
//! Items play in order. A cached clip starts on the very next frame; a streaming TTS item
//! starts once a little of it is buffered, and the items queued after it wait for it.
//!
//! The buffer is there because TTS streams come in bursts: eleven_v3 sends a third of a
//! second of speech, stops for half a second, then sends the rest faster than real time.
//! Played as it came, a sentence broke up mid-word ("קטוע"). Measured on Hebrew replies, a
//! 600 ms start covered most. eleven_v4_turbo, the voice now, streams steadily and several
//! times faster than real time (no buffer needed in any measured reply), so 250 ms are
//! enough and the 350 ms more were silence before every live sentence ("too slow", the
//! owner). A stream that still runs dry pauses once, for 250 ms, rather than stuttering.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::mulaw::{apply_gain, FRAME_BYTES, FRAME_MS, SILENCE};

/// Speech a stream buffers before it starts (μ-law 8 kHz: 8 bytes a millisecond).
const START_BUFFER_BYTES: usize = 250 * 8;
/// Speech a stream that ran dry mid-item buffers before it goes on.
const REBUFFER_BYTES: usize = 250 * 8;

pub enum Source {
    Clip(Bytes),
    /// Chunks as they arrive from a TTS provider; the channel closing ends the item.
    Stream(mpsc::Receiver<anyhow::Result<Bytes>>),
}

pub struct PlayItem {
    pub id: u64,
    pub source: Source,
    pub gain_db: f32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutFrame {
    /// One 20 ms frame (the last frame of an item is padded with silence).
    Audio(Bytes),
    /// Drop anything already buffered downstream (Twilio `clear`).
    Clear,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayoutEvent {
    /// First frame of an item left for the caller.
    Started { id: u64, at: Instant },
    /// Last frame of an item was sent.
    Finished { id: u64 },
    /// A streaming item failed; whatever arrived was played.
    Failed { id: u64, error: String },
    /// Queue empty and everything sent has had time to play.
    Idle,
    /// A cancel dropped these items (the current one first).
    Cancelled { ids: Vec<u64> },
}

enum Cmd {
    Enqueue(PlayItem),
    Cancel,
}

#[derive(Clone)]
pub struct Playout {
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl Playout {
    /// Start the playout task. `lead_frames` is how far ahead of real time to send (3 → 60 ms).
    pub fn spawn(
        out: mpsc::UnboundedSender<OutFrame>,
        events: mpsc::UnboundedSender<PlayoutEvent>,
        lead_frames: u32,
    ) -> (Self, JoinHandle<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(rx, out, events, lead_frames.max(1)));
        (Self { cmd: tx }, task)
    }

    pub fn enqueue(&self, item: PlayItem) {
        let _ = self.cmd.send(Cmd::Enqueue(item));
    }

    /// Stop now: drop the current item and everything queued.
    pub fn cancel(&self) {
        let _ = self.cmd.send(Cmd::Cancel);
    }
}

struct Active {
    id: u64,
    source: Source,
    offset: usize,
    pending: BytesMut,
    gain_db: f32,
    started: bool,
    stream_done: bool,
    /// Bytes to have before playing (again); 0 while playing.
    buffer_to: usize,
}

impl Active {
    fn new(item: PlayItem) -> Self {
        let buffer_to = if matches!(item.source, Source::Stream(_)) { START_BUFFER_BYTES } else { 0 };
        Self {
            id: item.id,
            source: item.source,
            offset: 0,
            pending: BytesMut::new(),
            gain_db: item.gain_db,
            started: false,
            stream_done: false,
            buffer_to,
        }
    }

    /// Next frame if one is available now. `None` with `done() == false` means waiting on
    /// a stream.
    fn next_frame(&mut self) -> Option<Bytes> {
        let mut frame = match &mut self.source {
            Source::Clip(clip) => {
                if self.offset >= clip.len() {
                    return None;
                }
                let end = (self.offset + FRAME_BYTES).min(clip.len());
                let mut f = BytesMut::from(&clip[self.offset..end]);
                self.offset = end;
                f.resize(FRAME_BYTES, SILENCE);
                f
            }
            Source::Stream(rx) => {
                while self.pending.len() < self.buffer_to.max(FRAME_BYTES) && !self.stream_done {
                    match rx.try_recv() {
                        Ok(Ok(chunk)) => self.pending.extend_from_slice(&chunk),
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "tts stream failed mid-item");
                            self.stream_done = true;
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => self.stream_done = true,
                    }
                }
                if self.buffer_to > 0 {
                    if self.pending.len() < self.buffer_to && !self.stream_done {
                        return None;
                    }
                    self.buffer_to = 0;
                }
                if self.pending.len() >= FRAME_BYTES {
                    self.pending.split_to(FRAME_BYTES)
                } else if self.stream_done && !self.pending.is_empty() {
                    let mut f = self.pending.split();
                    f.resize(FRAME_BYTES, SILENCE);
                    f
                } else {
                    if self.started {
                        tracing::info!(item = self.id, "live speech ran dry mid-item; buffering");
                        self.buffer_to = REBUFFER_BYTES;
                    }
                    return None;
                }
            }
        };
        apply_gain(&mut frame, self.gain_db);
        Some(frame.freeze())
    }

    fn done(&self) -> bool {
        match &self.source {
            Source::Clip(clip) => self.offset >= clip.len(),
            Source::Stream(_) => self.stream_done && self.pending.is_empty(),
        }
    }

    async fn wait_for_data(&mut self) {
        if let Source::Stream(rx) = &mut self.source {
            if self.stream_done {
                return;
            }
            match rx.recv().await {
                Some(Ok(chunk)) => self.pending.extend_from_slice(&chunk),
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "tts stream failed");
                    self.stream_done = true;
                }
                None => self.stream_done = true,
            }
        } else {
            std::future::pending::<()>().await;
        }
    }

    fn waiting_on_stream(&self) -> bool {
        matches!(self.source, Source::Stream(_))
            && !self.stream_done
            && self.pending.len() < self.buffer_to.max(FRAME_BYTES)
    }
}

async fn run(
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    out: mpsc::UnboundedSender<OutFrame>,
    events: mpsc::UnboundedSender<PlayoutEvent>,
    lead: u32,
) {
    let mut queue: VecDeque<PlayItem> = VecDeque::new();
    let mut current: Option<Active> = None;
    // Frames sent that have not yet had their 20 ms of real time.
    let mut ahead: u32 = 0;
    let mut idle_reported = true;
    let frame = Duration::from_millis(FRAME_MS);
    let mut tick = tokio::time::interval_at(Instant::now() + frame, frame);
    tick.set_missed_tick_behavior(MissedTickBehavior::Burst);

    loop {
        // Send as much as the lead allows.
        loop {
            if ahead >= lead {
                break;
            }
            if current.as_ref().is_none_or(Active::done) {
                if let Some(a) = current.take() {
                    let _ = events.send(PlayoutEvent::Finished { id: a.id });
                }
                match queue.pop_front() {
                    Some(item) => current = Some(Active::new(item)),
                    None => break,
                }
            }
            let Some(active) = current.as_mut() else { break };
            match active.next_frame() {
                Some(frame) => {
                    if !active.started {
                        active.started = true;
                        let _ = events.send(PlayoutEvent::Started { id: active.id, at: Instant::now() });
                    }
                    if out.send(OutFrame::Audio(frame)).is_err() {
                        return;
                    }
                    ahead += 1;
                    idle_reported = false;
                }
                None if active.done() => continue,
                None => break,
            }
        }
        if current.as_ref().is_some_and(Active::done) {
            if let Some(a) = current.take() {
                let _ = events.send(PlayoutEvent::Finished { id: a.id });
            }
        }
        if current.is_none() && queue.is_empty() && ahead == 0 && !idle_reported {
            idle_reported = true;
            let _ = events.send(PlayoutEvent::Idle);
        }

        let waiting = current.as_ref().is_some_and(Active::waiting_on_stream) && ahead < lead;
        // The select only decides what woke us; handling happens after it, when no branch
        // future borrows `current` any more.
        let wake = tokio::select! {
            biased;
            cmd = cmds.recv() => Wake::Cmd(cmd),
            () = async {
                match current.as_mut() {
                    Some(a) => a.wait_for_data().await,
                    None => std::future::pending().await,
                }
            }, if waiting => Wake::Data,
            _ = tick.tick() => Wake::Tick,
        };
        match wake {
            Wake::Cmd(None) => return,
            Wake::Cmd(Some(Cmd::Enqueue(item))) => queue.push_back(item),
            Wake::Cmd(Some(Cmd::Cancel)) => {
                let mut ids: Vec<u64> = current.take().map(|a| a.id).into_iter().collect();
                ids.extend(queue.drain(..).map(|i| i.id));
                let had_audio = ahead > 0 || !ids.is_empty();
                ahead = 0;
                if had_audio {
                    let _ = out.send(OutFrame::Clear);
                }
                if !ids.is_empty() {
                    let _ = events.send(PlayoutEvent::Cancelled { ids });
                }
                if !idle_reported {
                    idle_reported = true;
                    let _ = events.send(PlayoutEvent::Idle);
                }
            }
            Wake::Data => {}
            Wake::Tick => ahead = ahead.saturating_sub(1),
        }
    }
}

enum Wake {
    Cmd(Option<Cmd>),
    Data,
    Tick,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(n: usize) -> Bytes {
        Bytes::from(vec![0x55u8; n * FRAME_BYTES])
    }

    async fn drain(rx: &mut mpsc::UnboundedReceiver<OutFrame>) -> Vec<OutFrame> {
        tokio::task::yield_now().await;
        let mut v = Vec::new();
        while let Ok(f) = rx.try_recv() {
            v.push(f);
        }
        v
    }

    #[tokio::test(start_paused = true)]
    async fn cached_clip_starts_immediately_and_is_paced() {
        let (out_tx, mut out) = mpsc::unbounded_channel();
        let (ev_tx, mut ev) = mpsc::unbounded_channel();
        let (p, _task) = Playout::spawn(out_tx, ev_tx, 3);
        p.enqueue(PlayItem { id: 1, source: Source::Clip(frames(10)), gain_db: 0.0 });
        let first = drain(&mut out).await;
        assert_eq!(first.len(), 3, "primed with the lead immediately");
        assert!(matches!(ev.recv().await, Some(PlayoutEvent::Started { id: 1, .. })));
        tokio::time::advance(Duration::from_millis(100)).await;
        let later = drain(&mut out).await;
        assert!((4..=6).contains(&later.len()), "about one frame per 20 ms, got {}", later.len());
        tokio::time::advance(Duration::from_millis(400)).await;
        drain(&mut out).await;
        let mut saw_finished = false;
        let mut saw_idle = false;
        while let Ok(e) = ev.try_recv() {
            saw_finished |= e == PlayoutEvent::Finished { id: 1 };
            saw_idle |= e == PlayoutEvent::Idle;
        }
        assert!(saw_finished && saw_idle);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_clears_and_stops() {
        let (out_tx, mut out) = mpsc::unbounded_channel();
        let (ev_tx, mut ev) = mpsc::unbounded_channel();
        let (p, _task) = Playout::spawn(out_tx, ev_tx, 3);
        p.enqueue(PlayItem { id: 1, source: Source::Clip(frames(50)), gain_db: 0.0 });
        p.enqueue(PlayItem { id: 2, source: Source::Clip(frames(50)), gain_db: 0.0 });
        tokio::time::advance(Duration::from_millis(60)).await;
        drain(&mut out).await;
        p.cancel();
        let after = drain(&mut out).await;
        assert_eq!(after, vec![OutFrame::Clear]);
        tokio::time::advance(Duration::from_millis(200)).await;
        assert!(drain(&mut out).await.is_empty(), "nothing plays after a cancel");
        let mut cancelled = None;
        while let Ok(e) = ev.try_recv() {
            if let PlayoutEvent::Cancelled { ids } = e {
                cancelled = Some(ids);
            }
        }
        assert_eq!(cancelled, Some(vec![1, 2]));
    }

    #[tokio::test(start_paused = true)]
    async fn stream_waits_for_first_chunk_then_plays_in_order() {
        let (out_tx, mut out) = mpsc::unbounded_channel();
        let (ev_tx, _ev) = mpsc::unbounded_channel();
        let (p, _task) = Playout::spawn(out_tx, ev_tx, 3);
        let (tts_tx, tts_rx) = mpsc::channel(8);
        p.enqueue(PlayItem { id: 1, source: Source::Clip(frames(1)), gain_db: 0.0 });
        p.enqueue(PlayItem { id: 2, source: Source::Stream(tts_rx), gain_db: 0.0 });
        p.enqueue(PlayItem { id: 3, source: Source::Clip(Bytes::from(vec![0x11u8; FRAME_BYTES])), gain_db: 0.0 });
        assert_eq!(drain(&mut out).await.len(), 1, "the ack plays; the stream has nothing yet");
        tokio::time::advance(Duration::from_millis(200)).await;
        assert!(drain(&mut out).await.is_empty(), "item 3 waits for item 2");
        tts_tx.send(Ok(Bytes::from(vec![0x22u8; FRAME_BYTES + 10]))).await.unwrap();
        drop(tts_tx);
        tokio::time::advance(Duration::from_millis(100)).await;
        let got = drain(&mut out).await;
        let bytes: Vec<u8> =
            got.iter().filter_map(|f| if let OutFrame::Audio(b) = f { Some(b[0]) } else { None }).collect();
        assert_eq!(bytes, vec![0x22, 0x22, 0x11], "stream (padded tail) then the next item");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_buffers_before_it_plays_and_pauses_once_when_it_runs_dry() {
        let (out_tx, mut out) = mpsc::unbounded_channel();
        let (ev_tx, _ev) = mpsc::unbounded_channel();
        let (p, _task) = Playout::spawn(out_tx, ev_tx, 3);
        let (tts_tx, tts_rx) = mpsc::channel(64);
        p.enqueue(PlayItem { id: 1, source: Source::Stream(tts_rx), gain_db: 0.0 });
        // A fifth of a second, then nothing.
        tts_tx.send(Ok(frames(10))).await.unwrap();
        tokio::time::advance(Duration::from_millis(300)).await;
        assert!(drain(&mut out).await.is_empty(), "not enough to start");
        tts_tx.send(Ok(frames(3))).await.unwrap();
        tokio::time::advance(Duration::from_millis(20)).await;
        assert!(!drain(&mut out).await.is_empty(), "250 ms buffered: it plays");
        // It all plays, then the stream runs dry: it waits for 250 ms before going on.
        tokio::time::advance(Duration::from_millis(400)).await;
        drain(&mut out).await;
        tts_tx.send(Ok(frames(5))).await.unwrap();
        tokio::time::advance(Duration::from_millis(100)).await;
        assert!(drain(&mut out).await.is_empty(), "100 ms is not enough to go on");
        tts_tx.send(Ok(frames(8))).await.unwrap();
        tokio::time::advance(Duration::from_millis(20)).await;
        assert!(!drain(&mut out).await.is_empty(), "250 ms buffered: it goes on");
        // The end of a stream plays whatever is left, however short.
        tokio::time::advance(Duration::from_millis(500)).await;
        drain(&mut out).await;
        tts_tx.send(Ok(frames(2))).await.unwrap();
        drop(tts_tx);
        tokio::time::advance(Duration::from_millis(20)).await;
        assert!(!drain(&mut out).await.is_empty(), "the tail is not held back");
    }
}
