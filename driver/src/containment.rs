//! Containment (FR-PROC-06): Landlock and the seccomp filter, each `off`,
//! `best_effort` or `required` (FR-CFG-25), applied at startup.
//!
//! `restrict_files` (Landlock) runs right after the privilege drop,
//! while the process has one thread, so every thread started later is
//! restricted too. `restrict_calls` (seccomp) runs right before the
//! loop, once the disk workers run (`SECCOMP_FILTER_FLAG_TSYNC` covers
//! them), and settles what covers Landlock's gaps. Together they return
//! the outcomes the startup log states, one line per layer (FR-LOG-07);
//! a required layer that is missing or only partly supported, or a
//! layer the kernel offers but refuses to apply, is a startup failure
//! (exit 2).
//!
//! The decisions are pure (`files_plan`, `calls_plan`, `cover`), so
//! every branch is tested without a special kernel.

pub mod filter;
mod line;

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use durchlauferhitzer_core::config::{Containment, Layer, SeccompAction};

use self::filter::{Arch, program};
use crate::sys::landlock::{self, Abi};
use crate::sys::seccomp::{self, Personality, Support};

/// Why containment cannot start (exit 2): the message printed after
/// `durchlauferhitzer: `.
#[derive(Debug, PartialEq, Eq)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A containment layer (FR-PROC-06).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerName {
    Landlock,
    Seccomp,
}

/// What a layer does in this process (FR-PROC-06).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Applied, with nothing it should restrict left open (a gap
    /// another layer covers, or truncation, is no reason for less).
    Active(Applied),
    /// Applied, but a gap it should close is open (the outcome's
    /// `uncovered`): logged under `best_effort`, exit 2 under `required`.
    Partial(Applied),
    /// Not applied: the kernel (or this architecture) has none, under
    /// `best_effort`.
    Missing,
    /// Not applied: `off` in the config.
    Off,
}

/// How an applied layer applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Landlock at the kernel's ABI version.
    Abi(u32),
    /// The seccomp filter, with what an unlisted call gets.
    Filter(SeccompAction),
}

/// A right a layer cannot restrict here (FR-PROC-06).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gap {
    /// Landlock before ABI 3: truncation. Nothing covers it.
    Truncate,
    /// Landlock before ABI 5: `ioctl` on devices, covered by the seccomp
    /// filter (it allows no `ioctl`).
    DeviceIoctl,
    /// seccomp: the personality has `READ_IMPLIES_EXEC`, so a readable
    /// mapping is executable whatever the `PROT_EXEC` check says.
    ReadImpliesExec,
}

impl From<landlock::Gap> for Gap {
    fn from(gap: landlock::Gap) -> Self {
        match gap {
            landlock::Gap::Truncate => Self::Truncate,
            landlock::Gap::DeviceIoctl => Self::DeviceIoctl,
        }
    }
}

/// A layer's outcome, as the startup log states it (FR-LOG-05).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub layer: LayerName,
    /// How strictly the config asks for the layer (FR-PROC-08).
    pub mode: Layer,
    pub state: State,
    /// The gaps the seccomp filter covers.
    pub covered: Vec<Gap>,
    /// The gaps nothing covers.
    pub uncovered: Vec<Gap>,
}

impl Outcome {
    /// A layer that is not applied (`Off` or `Missing`): no gap to name.
    fn skipped(layer: LayerName, mode: Layer, state: State) -> Self {
        Self {
            layer,
            mode,
            state,
            covered: Vec::new(),
            uncovered: Vec::new(),
        }
    }

    /// An applied layer whose gaps are not covered (yet).
    fn applied(layer: LayerName, mode: Layer, state: State, uncovered: Vec<Gap>) -> Self {
        Self {
            layer,
            mode,
            state,
            covered: Vec::new(),
            uncovered,
        }
    }
}

/// Landlock's outcome as `restrict_files` leaves it, with the ABI it was
/// applied at (`None`: not applied), for `restrict_calls` to settle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Files {
    outcome: Outcome,
    abi: Option<u32>,
}

/// What to do with Landlock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilesPlan {
    /// Leave it out: `Off` or `Missing`.
    Skip(State),
    /// Restrict at this ABI.
    Apply(u32),
}

/// What to do with seccomp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallsPlan {
    /// Leave it out: `Off` or `Missing`.
    Skip(State),
    /// Install the filter for this architecture.
    Apply(Arch, Fit),
}

/// Whether the filter enforces all its checks in this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fit {
    Whole,
    /// `READ_IMPLIES_EXEC` defeats the `PROT_EXEC` check.
    Partial,
}

/// Landlock (FR-PROC-06): restricts the process to reading beneath
/// `roots`, at the kernel's ABI. Call it while the process has one
/// thread: threads already running are not restricted. The outcome's
/// gaps are all uncovered until [`restrict_calls`] settles them.
pub(crate) fn restrict_files(config: &Containment, roots: &[OwnedFd]) -> Result<Files, Error> {
    match files_plan(config.landlock, landlock::abi())? {
        FilesPlan::Skip(state) => Ok(Files {
            outcome: Outcome::skipped(LayerName::Landlock, config.landlock, state),
            abi: None,
        }),
        FilesPlan::Apply(abi) => {
            let roots: Vec<BorrowedFd<'_>> = roots.iter().map(AsFd::as_fd).collect();
            landlock::restrict_to_reading(&roots, abi)
                .map_err(|err| Error(format!("landlock: {err}")))?;
            let gaps = landlock::gaps(abi).into_iter().map(Gap::from).collect();
            Ok(Files {
                outcome: Outcome::applied(
                    LayerName::Landlock,
                    config.landlock,
                    State::Active(Applied::Abi(abi)),
                    gaps,
                ),
                abi: Some(abi),
            })
        }
    }
}

/// seccomp (FR-PROC-06): installs the allow-list for every thread, then
/// settles which of `landlock`'s gaps the filter covers. Returns both
/// outcomes, Landlock's first, in the order they are logged.
pub(crate) fn restrict_calls(config: &Containment, landlock: Files) -> Result<[Outcome; 2], Error> {
    let persona = || seccomp::personality().map_err(|err| Error(format!("personality: {err}")));
    let plan = calls_plan(
        config.seccomp,
        Arch::current(),
        seccomp::supported(),
        persona,
    )?;
    let calls = match plan {
        CallsPlan::Skip(state) => Outcome::skipped(LayerName::Seccomp, config.seccomp, state),
        CallsPlan::Apply(arch, fit) => {
            let action = config.seccomp_action;
            seccomp::install(&program(arch, action, std::process::id()))
                .map_err(|err| Error(format!("seccomp: {err}")))?;
            let applied = Applied::Filter(action);
            let mode = config.seccomp;
            match fit {
                Fit::Whole => {
                    Outcome::applied(LayerName::Seccomp, mode, State::Active(applied), vec![])
                }
                Fit::Partial => Outcome::applied(
                    LayerName::Seccomp,
                    mode,
                    State::Partial(applied),
                    vec![Gap::ReadImpliesExec],
                ),
            }
        }
    };
    let files = cover(config.landlock, landlock, calls.state)?;
    Ok([files, calls])
}

/// Landlock's decision: `layer` against the kernel's `abi`.
fn files_plan(layer: Layer, abi: Abi) -> Result<FilesPlan, Error> {
    match (layer, abi) {
        (Layer::Off, _) => Ok(FilesPlan::Skip(State::Off)),
        (Layer::BestEffort, Abi::Missing) => Ok(FilesPlan::Skip(State::Missing)),
        (Layer::Required, Abi::Missing) => Err(Error(
            "landlock is required but the kernel has none".to_owned(),
        )),
        (Layer::BestEffort | Layer::Required, Abi::Version(abi)) => Ok(FilesPlan::Apply(abi)),
    }
}

/// seccomp's decision: `layer` against the architecture's table, the
/// kernel's support and the personality, which is read (`persona`) only
/// when the filter is to be installed.
fn calls_plan(
    layer: Layer,
    arch: Option<Arch>,
    support: Support,
    persona: impl FnOnce() -> Result<Personality, Error>,
) -> Result<CallsPlan, Error> {
    let lacking = |why: &str| match layer {
        Layer::Required => Err(Error(format!("seccomp is required but {why}"))),
        Layer::Off | Layer::BestEffort => Ok(CallsPlan::Skip(State::Missing)),
    };
    match layer {
        Layer::Off => return Ok(CallsPlan::Skip(State::Off)),
        Layer::BestEffort | Layer::Required => {}
    }
    let Some(arch) = arch else {
        return lacking("it has no allow-list for this architecture");
    };
    match support {
        Support::Supported => {}
        Support::Missing => return lacking("the kernel has none"),
    }
    match (persona()?, layer) {
        (Personality::Plain, _) => Ok(CallsPlan::Apply(arch, Fit::Whole)),
        (Personality::ReadImpliesExec, Layer::Required) => Err(Error(
            "seccomp is required but the personality has READ_IMPLIES_EXEC, \
             which defeats its PROT_EXEC check"
                .to_owned(),
        )),
        (Personality::ReadImpliesExec, Layer::Off | Layer::BestEffort) => {
            Ok(CallsPlan::Apply(arch, Fit::Partial))
        }
    }
}

/// Moves `landlock`'s device-`ioctl` gap to `covered` when the filter is
/// installed (`seccomp` active or partial); otherwise it makes Landlock
/// partial, a failure under `required`. Truncation stays uncovered and
/// changes nothing (owner decision 2026-11-01).
fn cover(layer: Layer, files: Files, seccomp: State) -> Result<Outcome, Error> {
    let Files {
        outcome: mut landlock,
        abi,
    } = files;
    // Only an applied Landlock has gaps.
    let Some(abi) = abi else {
        return Ok(landlock);
    };
    let Some(at) = landlock
        .uncovered
        .iter()
        .position(|&gap| gap == Gap::DeviceIoctl)
    else {
        return Ok(landlock);
    };
    let seccomp = match seccomp {
        State::Active(_) | State::Partial(_) => {
            let gap = landlock.uncovered.remove(at);
            landlock.covered.push(gap);
            return Ok(landlock);
        }
        State::Off => "off",
        State::Missing => "missing",
    };
    match layer {
        Layer::Required => Err(Error(format!(
            "landlock is required but the kernel's Landlock (ABI {abi}) cannot \
             restrict device_ioctl, and seccomp, which covers it, is {seccomp}"
        ))),
        Layer::Off | Layer::BestEffort => {
            landlock.state = State::Partial(Applied::Abi(abi));
            Ok(landlock)
        }
    }
}

#[cfg(test)]
mod tests;
