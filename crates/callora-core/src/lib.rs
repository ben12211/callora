//! Callora's generic conversation runtime.
//!
//! This crate knows about businesses, intents, pipelines, slots, rules, actions and
//! responses, and nothing about telephony, audio or network providers. Every function is
//! deterministic given its inputs, which is what makes the conversation behaviour
//! testable without a phone line.

pub mod agent;
pub mod business;
pub mod config;
pub mod customer;
pub mod engine;
pub mod hebrew;
pub mod llm;
pub mod render;
pub mod speech;
pub mod state;
pub mod text;
pub mod time;
pub mod understanding;
pub mod values;
