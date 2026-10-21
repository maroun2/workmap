//! Host hook adapter: reads one documented hook event (JSON on stdin) and prints
//! bounded, fact-based context or nothing. Never calls the network or a model.

use crate::config::Env;
use rusqlite::Connection;

pub fn run(env: &Env, c: &Connection, event: &str, harness: &str) -> Result<(), String> {
    let _ = (env, c);
    Err(format!("hook {event} for {harness}: not implemented yet"))
}
