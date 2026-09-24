//! Adapters to external services. Each one implements a port from `callora-runtime` (or
//! `callora-audio` for TTS), so the runtime never depends on a specific vendor.

pub mod cartesia;
pub mod elevenlabs;
pub mod openai;
pub mod twilio_rest;
