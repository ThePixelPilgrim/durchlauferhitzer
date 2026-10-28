//! The disk worker pool (A10, DRV-07…09).
//!
//! Stage 4 Task 5: a stub that takes no job and answers nothing, so the
//! event loop can be built and tested with configs that have no static
//! roots (the core then never opens a file). Task 6 replaces it with the
//! real pool (`Pool::start`, `submit`, `results`, `shutdown`).

/// The disk worker pool. A stub until Task 6: there is no way to build
/// one yet, so a [`crate::Driver`] runs with `None`, which it accepts
/// only for a core without static roots (DRV-09).
#[derive(Debug)]
pub struct Pool {
    _not_yet: (),
}
