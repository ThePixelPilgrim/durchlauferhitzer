//! A layer's outcome as its startup log line (FR-LOG-05): containment
//! renders its own tokens, so the log format needs none of its types.

use std::io::Write as _;

use durchlauferhitzer_core::config::{Layer, SeccompAction};

use super::{Applied, Gap, LayerName, Outcome, State};

impl Outcome {
    /// `kind=containment layer mode state [abi | action log] [gaps
    /// covered_by] [uncovered]`, without the newline: a list of gaps is
    /// joined by `+`.
    pub(crate) fn write_line(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"kind=containment layer=");
        out.extend_from_slice(match self.layer {
            LayerName::Landlock => b"landlock",
            LayerName::Seccomp => b"seccomp",
        });
        out.extend_from_slice(match self.mode {
            Layer::Off => b" mode=off",
            Layer::BestEffort => b" mode=best_effort",
            Layer::Required => b" mode=required",
        });
        let (state, applied): (&[u8], _) = match self.state {
            State::Active(applied) => (b"active", Some(applied)),
            State::Partial(applied) => (b"partial", Some(applied)),
            State::Missing => (b"missing", None),
            State::Off => (b"off", None),
        };
        out.extend_from_slice(b" state=");
        out.extend_from_slice(state);
        match applied {
            // Writing to a `Vec` never fails.
            Some(Applied::Abi(abi)) => {
                let _ = write!(out, " abi={abi}");
            }
            Some(Applied::Filter(action)) => {
                out.extend_from_slice(match action {
                    SeccompAction::Fail => b" action=fail",
                    SeccompAction::Kill => b" action=kill",
                });
                // Every blocked call is logged by the kernel (FR-PROC-06).
                out.extend_from_slice(b" log=kernel");
            }
            None => {}
        }
        if !self.covered.is_empty() {
            gaps(out, "gaps", &self.covered);
            out.extend_from_slice(b" covered_by=seccomp");
        }
        if !self.uncovered.is_empty() {
            gaps(out, "uncovered", &self.uncovered);
        }
    }
}

/// ` <key>=<gap>+<gap>...`.
fn gaps(out: &mut Vec<u8>, key: &str, gaps: &[Gap]) {
    let _ = write!(out, " {key}=");
    for (i, gap) in gaps.iter().enumerate() {
        if i > 0 {
            out.push(b'+');
        }
        out.extend_from_slice(match gap {
            Gap::Truncate => b"truncate",
            Gap::DeviceIoctl => b"device_ioctl",
            Gap::ReadImpliesExec => b"read_implies_exec",
        });
    }
}
