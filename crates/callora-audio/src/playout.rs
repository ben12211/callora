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
//!
//! Beyond pacing, the playout also:
//! - applies gain through a [`Limiter`] (loudness without clipping);
//! - can trim the silence TTS puts at both ends of a sentence, so sentences of one reply
//!   join without a hole;
//! - starts a sentence that follows another one with a smaller buffer than the first of a
//!   reply, because a gap inside a reply is heard and a first sentence's wait is not;
//! - measures the silence between the audio it sends ([`PlayoutEvent::Gap`]);
//! - on a *soft* cancel lets a sentence that is nearly over finish
//!   ([`PlayoutEvent::Protected`]) instead of cutting its last word.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::mulaw::{speech_bounds, Limiter, FRAME_BYTES, FRAME_MS, SILENCE};

/// μ-law 8 kHz: 8 bytes a millisecond.
const BYTES_PER_MS: usize = 8;

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

/// Silence at the ends of a sentence is cut down to `pad_ms` before the first sound and
/// `tail_pad_ms` after the last (the tail keeps more: Hebrew word endings fade out softly).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrimConfig {
    /// A 10 ms window quieter than this (RMS, 16-bit scale) is silence.
    pub threshold_rms: f32,
    pub pad_ms: u64,
    pub tail_pad_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct PlayoutConfig {
    /// How far ahead of real time to send (3 → 60 ms).
    pub lead_frames: u32,
    /// The loudest the output may be once gain is applied (dBFS).
    pub limiter_ceiling_dbfs: f32,
    /// Speech a stream buffers before it starts, when it opens a reply.
    pub start_buffer_ms: u32,
    /// The same for a stream that follows other audio of the same reply.
    pub continuation_buffer_ms: u32,
    /// Speech a stream that ran dry mid-item buffers before it goes on.
    pub rebuffer_ms: u32,
    /// `None`: audio is played as it is.
    pub trim: Option<TrimConfig>,
}

impl Default for PlayoutConfig {
    fn default() -> Self {
        Self {
            lead_frames: 3,
            limiter_ceiling_dbfs: -1.5,
            start_buffer_ms: 250,
            continuation_buffer_ms: 250,
            rebuffer_ms: 250,
            trim: None,
        }
    }
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
    /// A cancel dropped these items (the current one first). `remaining_ms` is the audio
    /// that was still to play (in flight, in the current item, and in queued clips; a queued
    /// stream's length is not known and not counted).
    Cancelled { ids: Vec<u64>, remaining_ms: u64 },
    /// A soft cancel found the current item nearly over and let it finish; the items queued
    /// after it were dropped (and reported as `Cancelled`).
    Protected { id: u64, remaining_ms: u64 },
    /// Real-time silence between two pieces of audio that were sent: the caller heard `ms`
    /// of nothing. `seam` is true when it fell between items, false inside one (a stream that
    /// ran dry). The first audio after a pause is reported too; whether that was a pause or
    /// a hole is for the runtime to say.
    Gap { id: u64, ms: u64, seam: bool },
}

enum Cmd {
    Enqueue(PlayItem),
    Cancel,
    /// Cancel, unless the current item has at most this much left to play.
    CancelSoft {
        protect_ms: u64,
    },
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
        Self::spawn_with(out, events, PlayoutConfig { lead_frames, ..PlayoutConfig::default() })
    }

    pub fn spawn_with(
        out: mpsc::UnboundedSender<OutFrame>,
        events: mpsc::UnboundedSender<PlayoutEvent>,
        cfg: PlayoutConfig,
    ) -> (Self, JoinHandle<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(rx, out, events, PlayoutConfig { lead_frames: cfg.lead_frames.max(1), ..cfg }));
        (Self { cmd: tx }, task)
    }

    pub fn enqueue(&self, item: PlayItem) {
        let _ = self.cmd.send(Cmd::Enqueue(item));
    }

    /// Stop now: drop the current item and everything queued.
    pub fn cancel(&self) {
        let _ = self.cmd.send(Cmd::Cancel);
    }

    /// Stop, but let the current item finish when it has `protect` or less left to play (a
    /// sentence a word from its end is not cut off). Everything queued after it is dropped
    /// either way.
    pub fn cancel_soft(&self, protect: Duration) {
        let _ = self.cmd.send(Cmd::CancelSoft { protect_ms: protect.as_millis() as u64 });
    }
}

struct Active {
    id: u64,
    source: Source,
    offset: usize,
    pending: BytesMut,
    gain_db: f32,
    limiter: Limiter,
    started: bool,
    stream_done: bool,
    /// Bytes to have before playing (again); 0 while playing.
    buffer_to: usize,
    rebuffer_bytes: usize,
    trim: Option<TrimConfig>,
    head_trimmed: bool,
    tail_trimmed: bool,
}

impl Active {
    fn new(item: PlayItem, cfg: &PlayoutConfig, continuation: bool) -> Self {
        let start_ms = if continuation { cfg.continuation_buffer_ms } else { cfg.start_buffer_ms };
        let mut source = item.source;
        // Without trimming there is nothing to wait for before the head.
        let mut head_trimmed = cfg.trim.is_none();
        if let (Source::Clip(clip), Some(t)) = (&mut source, cfg.trim) {
            if let Some((a, b)) = speech_bounds(clip, t.threshold_rms, t.pad_ms, t.tail_pad_ms) {
                *clip = clip.slice(a..b);
            }
            head_trimmed = true;
        }
        let buffer_to = if matches!(source, Source::Stream(_)) { start_ms as usize * BYTES_PER_MS } else { 0 };
        Self {
            id: item.id,
            source,
            offset: 0,
            pending: BytesMut::new(),
            gain_db: item.gain_db,
            limiter: Limiter::new(cfg.limiter_ceiling_dbfs),
            started: false,
            stream_done: false,
            buffer_to,
            rebuffer_bytes: cfg.rebuffer_ms as usize * BYTES_PER_MS,
            trim: cfg.trim,
            head_trimmed,
            tail_trimmed: false,
        }
    }

    /// Take whatever the stream has sent so far, up to `up_to` bytes buffered.
    fn drain(&mut self, up_to: usize) {
        let Source::Stream(rx) = &mut self.source else { return };
        while self.pending.len() < up_to && !self.stream_done {
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
    }

    /// The leading silence of a stream is dropped once enough of it has arrived to tell
    /// where the speech starts.
    fn trim_head(&mut self) {
        let Some(t) = self.trim else { return };
        if self.head_trimmed || self.pending.is_empty() {
            return;
        }
        match speech_bounds(&self.pending, t.threshold_rms, t.pad_ms, t.tail_pad_ms) {
            Some((start, _)) => {
                let _ = self.pending.split_to(start);
                self.head_trimmed = true;
            }
            None if self.stream_done => self.head_trimmed = true,
            None => {
                // Still silent: keep only the pad.
                let keep = (t.pad_ms as usize * BYTES_PER_MS).min(self.pending.len());
                let drop = self.pending.len() - keep;
                let _ = self.pending.split_to(drop);
            }
        }
    }

    /// The silence after the last word is cut once the whole stream is in hand.
    fn trim_tail(&mut self) {
        let Some(t) = self.trim else { return };
        if self.tail_trimmed || !self.stream_done {
            return;
        }
        self.tail_trimmed = true;
        if let Some((_, end)) = speech_bounds(&self.pending, t.threshold_rms, t.pad_ms, t.tail_pad_ms) {
            self.pending.truncate(end);
        }
    }

    /// Audio still to play from this item, in bytes, and whether that is all of it (a stream
    /// still arriving is only counted as far as it has come).
    fn remaining(&mut self) -> (usize, bool) {
        match &self.source {
            Source::Clip(clip) => (clip.len().saturating_sub(self.offset), true),
            Source::Stream(_) => {
                self.drain(usize::MAX);
                (self.pending.len(), self.stream_done)
            }
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
            Source::Stream(_) => {
                self.drain(self.buffer_to.max(FRAME_BYTES));
                if !self.head_trimmed && (self.pending.len() >= self.buffer_to.max(FRAME_BYTES) || self.stream_done) {
                    self.trim_head();
                }
                self.trim_tail();
                if self.buffer_to > 0 {
                    if (self.pending.len() < self.buffer_to || !self.head_trimmed) && !self.stream_done {
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
                    if self.started && !self.stream_done {
                        tracing::info!(item = self.id, "live speech ran dry mid-item; buffering");
                        self.buffer_to = self.rebuffer_bytes;
                    }
                    return None;
                }
            }
        };
        self.limiter.process(&mut frame, self.gain_db);
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
            && (self.pending.len() < self.buffer_to.max(FRAME_BYTES) || !self.head_trimmed)
    }
}

/// The length of a queued item, when it is known.
fn queued_ms(item: &PlayItem) -> u64 {
    match &item.source {
        Source::Clip(c) => (c.len() / BYTES_PER_MS) as u64,
        Source::Stream(_) => 0,
    }
}

async fn run(
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    out: mpsc::UnboundedSender<OutFrame>,
    events: mpsc::UnboundedSender<PlayoutEvent>,
    cfg: PlayoutConfig,
) {
    let lead = cfg.lead_frames;
    let mut queue: VecDeque<PlayItem> = VecDeque::new();
    let mut current: Option<Active> = None;
    // Frames sent that have not yet had their 20 ms of real time.
    let mut ahead: u32 = 0;
    let mut idle_reported = true;
    // When everything sent so far will have been played (None: nothing sent since a clear).
    let mut play_end: Option<Instant> = None;
    // The item a soft cancel let finish.
    let mut protected: Option<u64> = None;
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
                    // After other audio of this reply (nothing has gone idle in between) the
                    // sentence needs less of a head start: a hole in the reply costs more.
                    Some(item) => current = Some(Active::new(item, &cfg, !idle_reported)),
                    None => break,
                }
            }
            let Some(active) = current.as_mut() else { break };
            match active.next_frame() {
                Some(frame) => {
                    let now = Instant::now();
                    if let Some(end) = play_end {
                        if now > end + Duration::from_millis(1) {
                            let ms = now.saturating_duration_since(end).as_millis() as u64;
                            let _ = events.send(PlayoutEvent::Gap { id: active.id, ms, seam: !active.started });
                        }
                    }
                    play_end = Some(play_end.map_or(now, |e| e.max(now)) + Duration::from_millis(FRAME_MS));
                    if !active.started {
                        active.started = true;
                        let _ = events.send(PlayoutEvent::Started { id: active.id, at: now });
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
        if current.is_none() {
            protected = None;
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
            Wake::Cmd(Some(cmd @ (Cmd::Cancel | Cmd::CancelSoft { .. }))) => {
                let in_flight = u64::from(ahead) * FRAME_MS;
                let queued: u64 = queue.iter().map(queued_ms).sum();
                let (current_left, exact) = current.as_mut().map_or((0, true), |a| {
                    let (b, exact) = a.remaining();
                    ((b / BYTES_PER_MS) as u64, exact)
                });
                let mut hard = true;
                if let Cmd::CancelSoft { protect_ms } = cmd {
                    // Only the item playing now can be let finish, and only when all of it
                    // is known (a stream still arriving has an unknown end).
                    if exact && current_left + in_flight <= protect_ms {
                        hard = false;
                        if let Some(a) = current.as_ref() {
                            if protected != Some(a.id) {
                                protected = Some(a.id);
                                let _ = events
                                    .send(PlayoutEvent::Protected { id: a.id, remaining_ms: current_left + in_flight });
                            }
                        }
                        let ids: Vec<u64> = queue.drain(..).map(|i| i.id).collect();
                        if !ids.is_empty() {
                            let _ = events.send(PlayoutEvent::Cancelled { ids, remaining_ms: queued });
                        }
                    }
                }
                if hard {
                    let mut ids: Vec<u64> = current.take().map(|a| a.id).into_iter().collect();
                    ids.extend(queue.drain(..).map(|i| i.id));
                    let had_audio = ahead > 0 || !ids.is_empty();
                    ahead = 0;
                    play_end = None;
                    protected = None;
                    if had_audio {
                        let _ = out.send(OutFrame::Clear);
                    }
                    if !ids.is_empty() {
                        let _ = events
                            .send(PlayoutEvent::Cancelled { ids, remaining_ms: current_left + in_flight + queued });
                    }
                    if !idle_reported {
                        idle_reported = true;
                        let _ = events.send(PlayoutEvent::Idle);
                    }
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
            if let PlayoutEvent::Cancelled { ids, .. } = e {
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

    // ---- gain, limiter, trimming, gaps, soft cancel ----

    use crate::mulaw::{decode, encode};

    fn spawn_cfg(
        cfg: PlayoutConfig,
    ) -> (Playout, mpsc::UnboundedReceiver<OutFrame>, mpsc::UnboundedReceiver<PlayoutEvent>) {
        let (out_tx, out) = mpsc::unbounded_channel();
        let (ev_tx, ev) = mpsc::unbounded_channel();
        let (p, _task) = Playout::spawn_with(out_tx, ev_tx, cfg);
        (p, out, ev)
    }

    fn events(ev: &mut mpsc::UnboundedReceiver<PlayoutEvent>) -> Vec<PlayoutEvent> {
        let mut v = Vec::new();
        while let Ok(e) = ev.try_recv() {
            v.push(e);
        }
        v
    }

    /// Let `ms` of real time pass in frame-sized steps, returning everything sent meanwhile.
    async fn run_for(out: &mut mpsc::UnboundedReceiver<OutFrame>, ms: u64) -> Vec<OutFrame> {
        let mut all = Vec::new();
        for _ in 0..ms / 20 {
            tokio::time::advance(Duration::from_millis(20)).await;
            all.extend(drain(out).await);
        }
        all
    }

    fn audio_frames(v: &[OutFrame]) -> usize {
        v.iter().filter(|f| matches!(f, OutFrame::Audio(_))).count()
    }

    #[tokio::test(start_paused = true)]
    async fn gain_is_limited_at_the_ceiling_and_zero_gain_is_untouched() {
        let tone: Vec<u8> =
            (0..FRAME_BYTES * 4).map(|i| encode(if (i / 4) % 2 == 0 { 12000 } else { -12000 })).collect();
        for (gain, untouched) in [(0.0f32, true), (12.0, false)] {
            let (p, mut out, _ev) = spawn_cfg(PlayoutConfig::default());
            p.enqueue(PlayItem { id: 1, source: Source::Clip(Bytes::from(tone.clone())), gain_db: gain });
            let mut sent = drain(&mut out).await;
            sent.extend(run_for(&mut out, 200).await);
            let played: Vec<u8> = sent
                .into_iter()
                .filter_map(|f| if let OutFrame::Audio(b) = f { Some(b.to_vec()) } else { None })
                .flatten()
                .collect();
            assert_eq!(played.len(), tone.len());
            if untouched {
                assert_eq!(played, tone, "0 dB is bit-exact");
            } else {
                let peak = played.iter().map(|b| i32::from(decode(*b)).abs()).max().unwrap();
                let ceiling = 32124.0 * 10f32.powf(-1.5 / 20.0);
                assert!(peak as f32 <= ceiling * 1.03, "{peak} passes the ceiling");
                assert!(peak > 12000, "and it is louder");
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn silence_at_the_ends_of_a_clip_is_trimmed_when_asked() {
        let mut clip = vec![SILENCE; 400 * 8];
        clip.extend(vec![encode(5000); 200 * 8]);
        clip.extend(vec![SILENCE; 400 * 8]);
        let trim = Some(TrimConfig { threshold_rms: 100.0, pad_ms: 20, tail_pad_ms: 20 });
        let (p, mut out, _ev) = spawn_cfg(PlayoutConfig { trim, ..PlayoutConfig::default() });
        p.enqueue(PlayItem { id: 1, source: Source::Clip(Bytes::from(clip.clone())), gain_db: 0.0 });
        let n = audio_frames(&drain(&mut out).await) + audio_frames(&run_for(&mut out, 1200).await);
        assert_eq!(n, 12, "20 ms + 200 ms + 20 ms, not a second");
        let (p, mut out, _ev) = spawn_cfg(PlayoutConfig::default());
        p.enqueue(PlayItem { id: 1, source: Source::Clip(Bytes::from(clip)), gain_db: 0.0 });
        let n = audio_frames(&drain(&mut out).await) + audio_frames(&run_for(&mut out, 1200).await);
        assert_eq!(n, 50, "untouched without trimming");
    }

    #[tokio::test(start_paused = true)]
    async fn segments_of_one_reply_play_without_a_gap() {
        let (p, mut out, mut ev) = spawn_cfg(PlayoutConfig::default());
        let (tts_tx, tts_rx) = mpsc::channel(64);
        p.enqueue(PlayItem { id: 1, source: Source::Clip(frames(10)), gain_db: 0.0 });
        p.enqueue(PlayItem { id: 2, source: Source::Stream(tts_rx), gain_db: 0.0 });
        p.enqueue(PlayItem { id: 3, source: Source::Clip(frames(10)), gain_db: 0.0 });
        // The live sentence is ready long before the clip before it ends.
        tts_tx.send(Ok(frames(20))).await.unwrap();
        drop(tts_tx);
        let total = audio_frames(&drain(&mut out).await) + audio_frames(&run_for(&mut out, 1000).await);
        assert_eq!(total, 40, "10 + 20 + 10 frames, nothing missing");
        let gaps: Vec<_> = events(&mut ev).into_iter().filter(|e| matches!(e, PlayoutEvent::Gap { .. })).collect();
        assert!(gaps.is_empty(), "no silence between the segments: {gaps:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_segment_is_reported_as_a_gap_between_items() {
        let (p, mut out, mut ev) = spawn_cfg(PlayoutConfig::default());
        let (tts_tx, tts_rx) = mpsc::channel(64);
        p.enqueue(PlayItem { id: 1, source: Source::Clip(frames(5)), gain_db: 0.0 });
        p.enqueue(PlayItem { id: 2, source: Source::Stream(tts_rx), gain_db: 0.0 });
        drain(&mut out).await;
        run_for(&mut out, 400).await;
        tts_tx.send(Ok(frames(20))).await.unwrap();
        run_for(&mut out, 40).await;
        let gap = events(&mut ev).into_iter().find_map(|e| match e {
            PlayoutEvent::Gap { id: 2, ms, seam } => Some((ms, seam)),
            _ => None,
        });
        let (ms, seam) = gap.expect("a gap before item 2");
        assert!(seam && (250..=330).contains(&ms), "about 300 ms of nothing between the items, got {ms}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_sentence_that_follows_audio_needs_less_of_a_head_start() {
        let cfg = PlayoutConfig { continuation_buffer_ms: 40, ..PlayoutConfig::default() };
        let (p, mut out, _ev) = spawn_cfg(cfg);
        let (tts_tx, tts_rx) = mpsc::channel(64);
        p.enqueue(PlayItem { id: 1, source: Source::Clip(frames(3)), gain_db: 0.0 });
        p.enqueue(PlayItem { id: 2, source: Source::Stream(tts_rx), gain_db: 0.0 });
        tts_tx.send(Ok(frames(2))).await.unwrap();
        drop(tts_tx);
        let n = audio_frames(&drain(&mut out).await) + audio_frames(&run_for(&mut out, 200).await);
        assert_eq!(n, 5, "40 ms are enough after other audio");
        // After a pause it is the full 250 ms again.
        run_for(&mut out, 500).await;
        let (tts_tx2, tts_rx2) = mpsc::channel(64);
        p.enqueue(PlayItem { id: 3, source: Source::Stream(tts_rx2), gain_db: 0.0 });
        tts_tx2.send(Ok(frames(2))).await.unwrap();
        assert!(run_for(&mut out, 100).await.is_empty(), "a reply's first sentence still buffers");
    }

    #[tokio::test(start_paused = true)]
    async fn a_soft_cancel_lets_a_nearly_finished_sentence_end() {
        let (p, mut out, mut ev) = spawn_cfg(PlayoutConfig::default());
        p.enqueue(PlayItem { id: 1, source: Source::Clip(frames(20)), gain_db: 0.0 });
        p.enqueue(PlayItem { id: 2, source: Source::Clip(frames(20)), gain_db: 0.0 });
        let mut played = audio_frames(&drain(&mut out).await) + audio_frames(&run_for(&mut out, 300).await);
        p.cancel_soft(Duration::from_millis(500));
        let after = run_for(&mut out, 600).await;
        assert!(!after.contains(&OutFrame::Clear), "nothing is cut");
        played += audio_frames(&after);
        assert_eq!(played, 20, "the sentence plays to its end, the next one never starts");
        let evs = events(&mut ev);
        assert!(evs.iter().any(|e| matches!(e, PlayoutEvent::Protected { id: 1, .. })));
        assert!(evs.iter().any(|e| matches!(e, PlayoutEvent::Cancelled { ids, .. } if ids == &vec![2])));
        assert!(evs.contains(&PlayoutEvent::Idle));
    }

    #[tokio::test(start_paused = true)]
    async fn a_soft_cancel_far_from_the_end_cuts_at_once() {
        let (p, mut out, mut ev) = spawn_cfg(PlayoutConfig::default());
        p.enqueue(PlayItem { id: 1, source: Source::Clip(frames(50)), gain_db: 0.0 });
        drain(&mut out).await;
        run_for(&mut out, 100).await;
        p.cancel_soft(Duration::from_millis(500));
        assert_eq!(drain(&mut out).await, vec![OutFrame::Clear]);
        let cancelled = events(&mut ev).into_iter().find_map(|e| match e {
            PlayoutEvent::Cancelled { ids, remaining_ms } => Some((ids, remaining_ms)),
            _ => None,
        });
        let (ids, remaining) = cancelled.unwrap();
        assert_eq!(ids, vec![1]);
        assert!((800..=900).contains(&remaining), "about 850 ms were left, got {remaining}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_soft_cancel_on_a_stream_still_arriving_cuts_because_its_end_is_unknown() {
        let (p, mut out, _ev) = spawn_cfg(PlayoutConfig::default());
        let (tts_tx, tts_rx) = mpsc::channel(64);
        p.enqueue(PlayItem { id: 1, source: Source::Stream(tts_rx), gain_db: 0.0 });
        tts_tx.send(Ok(frames(15))).await.unwrap();
        drain(&mut out).await;
        run_for(&mut out, 260).await;
        p.cancel_soft(Duration::from_millis(5_000));
        assert_eq!(drain(&mut out).await, vec![OutFrame::Clear], "more may still come: cut");
        drop(tts_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn a_soft_cancel_on_a_finished_stream_near_its_end_lets_it_finish() {
        let (p, mut out, mut ev) = spawn_cfg(PlayoutConfig::default());
        let (tts_tx, tts_rx) = mpsc::channel(64);
        p.enqueue(PlayItem { id: 1, source: Source::Stream(tts_rx), gain_db: 0.0 });
        tts_tx.send(Ok(frames(15))).await.unwrap();
        drop(tts_tx);
        let mut played = audio_frames(&drain(&mut out).await) + audio_frames(&run_for(&mut out, 200).await);
        p.cancel_soft(Duration::from_millis(5_000));
        let rest = run_for(&mut out, 400).await;
        assert!(!rest.contains(&OutFrame::Clear));
        played += audio_frames(&rest);
        assert_eq!(played, 15);
        assert!(events(&mut ev).iter().any(|e| matches!(e, PlayoutEvent::Protected { id: 1, .. })));
    }
}
