//! Adapters to external services. Each one implements a port from `callora-runtime` (or
//! `callora-audio` for TTS), so the runtime never depends on a specific vendor.

pub mod cartesia;
pub mod deepgram;
pub mod elevenlabs;
pub mod openai;
pub mod openai_audio;
pub mod openai_stt;
pub mod race;
pub mod scribe;
pub mod scribe_batch;
pub mod twilio_rest;
