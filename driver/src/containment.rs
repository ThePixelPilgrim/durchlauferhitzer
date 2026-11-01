//! Containment (FR-PROC-06): the seccomp allow-list generator for now;
//! startup wires Landlock and seccomp in here later.

// Used by its tests until the startup calls it.
#[cfg_attr(not(test), expect(dead_code, reason = "startup calls it next"))]
mod filter;
