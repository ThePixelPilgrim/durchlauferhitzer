//! The softer containment choices a config makes (FR-PROC-08): what
//! `--check` warns about, one line each.

use std::fmt;

use super::model::{Containment, Layer, SeccompAction};

/// A containment setting written softer than its default (FR-CFG-25):
/// since the defaults are the strict choices, each names an explicit
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Softening {
    /// `landlock best_effort`.
    LandlockBestEffort,
    /// `landlock off`.
    LandlockOff,
    /// `seccomp best_effort`.
    SeccompBestEffort,
    /// `seccomp off`.
    SeccompOff,
    /// `seccomp_action fail`.
    SeccompActionFail,
}

impl Containment {
    /// The softer choices of this block, in config order (`landlock`,
    /// `seccomp`, `seccomp_action`); empty for the defaults.
    pub fn softenings(&self) -> Vec<Softening> {
        let landlock = match self.landlock {
            Layer::Off => Some(Softening::LandlockOff),
            Layer::BestEffort => Some(Softening::LandlockBestEffort),
            Layer::Required => None,
        };
        let seccomp = match self.seccomp {
            Layer::Off => Some(Softening::SeccompOff),
            Layer::BestEffort => Some(Softening::SeccompBestEffort),
            Layer::Required => None,
        };
        let action = match self.seccomp_action {
            SeccompAction::Fail => Some(Softening::SeccompActionFail),
            SeccompAction::Kill => None,
        };
        [landlock, seccomp, action].into_iter().flatten().collect()
    }
}

impl fmt::Display for Softening {
    /// The warning's message (FR-PROC-08), without the config path.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::LandlockBestEffort => {
                "containment landlock best_effort: serves on a kernel without Landlock"
            }
            Self::LandlockOff => "containment landlock off: Landlock is not used",
            Self::SeccompBestEffort => {
                "containment seccomp best_effort: serves on a kernel without seccomp"
            }
            Self::SeccompOff => "containment seccomp off: the system-call filter is not used",
            Self::SeccompActionFail => {
                "containment seccomp_action fail: an unexpected system call is answered, \
                 not fatal"
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_has_no_softening() {
        assert_eq!(Containment::default().softenings(), vec![]);
    }

    #[test]
    fn every_softening_in_config_order() {
        let containment = Containment {
            landlock: Layer::Off,
            seccomp: Layer::BestEffort,
            seccomp_action: SeccompAction::Fail,
        };
        assert_eq!(
            containment.softenings(),
            vec![
                Softening::LandlockOff,
                Softening::SeccompBestEffort,
                Softening::SeccompActionFail
            ]
        );
    }

    #[test]
    fn display_is_the_exact_line() {
        let cases = [
            (
                Softening::LandlockBestEffort,
                "containment landlock best_effort: serves on a kernel without Landlock",
            ),
            (
                Softening::LandlockOff,
                "containment landlock off: Landlock is not used",
            ),
            (
                Softening::SeccompBestEffort,
                "containment seccomp best_effort: serves on a kernel without seccomp",
            ),
            (
                Softening::SeccompOff,
                "containment seccomp off: the system-call filter is not used",
            ),
            (
                Softening::SeccompActionFail,
                "containment seccomp_action fail: an unexpected system call is answered, \
                 not fatal",
            ),
        ];
        for (softening, line) in cases {
            assert_eq!(softening.to_string(), line);
        }
    }
}
