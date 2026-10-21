//! Explicit, user-triggered interactive resume of a recorded conversation in its
//! own provider CLI. Distinct from the read-only `ask`.

use rusqlite::Connection;

pub fn run(c: &Connection) -> Result<(), String> {
    let _ = c;
    Err("resume: not implemented yet".into())
}
