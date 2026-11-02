//! The decisions of containment (FR-PROC-06), pure: every mode against
//! every kernel answer, without a special kernel.

use durchlauferhitzer_core::config::{Layer, SeccompAction};

use super::filter::Arch;
use super::{
    Applied, CallsPlan, Error, Files, FilesPlan, Fit, Gap, LayerName, Outcome, State, calls_plan,
    cover, files_plan,
};
use crate::sys::landlock::Abi;
use crate::sys::seccomp::{Personality, Support};

const MODES: [Layer; 3] = [Layer::Off, Layer::BestEffort, Layer::Required];

fn message<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
    match result {
        Err(Error(message)) => message,
        Ok(plan) => panic!("no failure: {plan:?}"),
    }
}

#[test]
fn landlock_off_is_off_whatever_the_kernel() {
    for abi in [Abi::Missing, Abi::Version(1), Abi::Version(7)] {
        assert_eq!(
            files_plan(Layer::Off, abi).unwrap(),
            FilesPlan::Skip(State::Off)
        );
    }
}

#[test]
fn landlock_missing_is_logged_under_best_effort_and_fatal_under_required() {
    assert_eq!(
        files_plan(Layer::BestEffort, Abi::Missing).unwrap(),
        FilesPlan::Skip(State::Missing)
    );
    assert_eq!(
        message(files_plan(Layer::Required, Abi::Missing)),
        "landlock is required but the kernel has none"
    );
}

#[test]
fn any_landlock_abi_is_applied_at_its_version() {
    for layer in [Layer::BestEffort, Layer::Required] {
        for abi in [1, 2, 3, 4, 5, 7] {
            assert_eq!(
                files_plan(layer, Abi::Version(abi)).unwrap(),
                FilesPlan::Apply(abi)
            );
        }
    }
}

const ARCH: Option<Arch> = Arch::current();

/// `calls_plan` with a personality that reads as `persona`.
fn calls(
    layer: Layer,
    arch: Option<Arch>,
    support: Support,
    persona: Personality,
) -> Result<CallsPlan, Error> {
    calls_plan(layer, arch, support, || Ok(persona))
}

fn plan(layer: Layer, arch: Option<Arch>, support: Support, persona: Personality) -> CallsPlan {
    calls(layer, arch, support, persona).unwrap()
}

/// A-3: the personality is read only on the way to installing the
/// filter, so a read that fails cannot fail a skipped layer.
#[test]
fn the_personality_is_read_only_when_the_filter_is_applied() {
    let unreadable = || Err(Error("personality: EPERM".to_owned()));
    for support in [Support::Supported, Support::Missing] {
        assert_eq!(
            calls_plan(Layer::Off, ARCH, support, unreadable).unwrap(),
            CallsPlan::Skip(State::Off)
        );
        assert_eq!(
            calls_plan(Layer::BestEffort, None, support, unreadable).unwrap(),
            CallsPlan::Skip(State::Missing)
        );
    }
    assert_eq!(
        calls_plan(Layer::BestEffort, ARCH, Support::Missing, unreadable).unwrap(),
        CallsPlan::Skip(State::Missing)
    );
    for layer in [Layer::BestEffort, Layer::Required] {
        assert_eq!(
            message(calls_plan(layer, ARCH, Support::Supported, unreadable)),
            "personality: EPERM"
        );
    }
}

#[test]
fn seccomp_off_is_off_whatever_the_kernel() {
    for support in [Support::Supported, Support::Missing] {
        for persona in [Personality::Plain, Personality::ReadImpliesExec] {
            for arch in [ARCH, None] {
                assert_eq!(
                    plan(Layer::Off, arch, support, persona),
                    CallsPlan::Skip(State::Off)
                );
            }
        }
    }
}

#[test]
fn seccomp_is_applied_whole_with_a_plain_personality() {
    let arch = ARCH.unwrap();
    for layer in [Layer::BestEffort, Layer::Required] {
        assert_eq!(
            plan(layer, ARCH, Support::Supported, Personality::Plain),
            CallsPlan::Apply(arch, Fit::Whole)
        );
    }
}

#[test]
fn seccomp_missing_in_the_kernel() {
    assert_eq!(
        plan(
            Layer::BestEffort,
            ARCH,
            Support::Missing,
            Personality::Plain
        ),
        CallsPlan::Skip(State::Missing)
    );
    assert_eq!(
        message(calls(
            Layer::Required,
            ARCH,
            Support::Missing,
            Personality::Plain
        )),
        "seccomp is required but the kernel has none"
    );
}

#[test]
fn seccomp_missing_for_the_architecture() {
    for support in [Support::Supported, Support::Missing] {
        assert_eq!(
            plan(Layer::BestEffort, None, support, Personality::Plain),
            CallsPlan::Skip(State::Missing)
        );
        assert_eq!(
            message(calls(Layer::Required, None, support, Personality::Plain)),
            "seccomp is required but it has no allow-list for this architecture"
        );
    }
}

#[test]
fn read_implies_exec_makes_seccomp_partial() {
    let arch = ARCH.unwrap();
    assert_eq!(
        plan(
            Layer::BestEffort,
            ARCH,
            Support::Supported,
            Personality::ReadImpliesExec
        ),
        CallsPlan::Apply(arch, Fit::Partial)
    );
    assert_eq!(
        message(calls(
            Layer::Required,
            ARCH,
            Support::Supported,
            Personality::ReadImpliesExec
        )),
        "seccomp is required but the personality has READ_IMPLIES_EXEC, \
         which defeats its PROT_EXEC check"
    );
}

/// What `restrict_files` returns for an applied ABI.
fn landlock(abi: u32) -> Files {
    Files {
        outcome: Outcome::applied(
            LayerName::Landlock,
            State::Active(Applied::Abi(abi)),
            abi_gaps(abi),
        ),
        abi: Some(abi),
    }
}

fn abi_gaps(abi: u32) -> Vec<Gap> {
    crate::sys::landlock::gaps(abi)
        .into_iter()
        .map(Gap::from)
        .collect()
}

const FILTER: Applied = Applied::Filter(SeccompAction::Fail);
const FILTERED: [State; 2] = [State::Active(FILTER), State::Partial(FILTER)];
const UNFILTERED: [State; 2] = [State::Off, State::Missing];

fn outcome(state: State, covered: &[Gap], uncovered: &[Gap]) -> Outcome {
    Outcome {
        layer: LayerName::Landlock,
        state,
        covered: covered.to_vec(),
        uncovered: uncovered.to_vec(),
    }
}

#[test]
fn a_landlock_that_is_not_applied_has_nothing_to_cover() {
    for layer in MODES {
        for state in [State::Off, State::Missing] {
            for seccomp in FILTERED.into_iter().chain(UNFILTERED) {
                let skipped = Outcome::skipped(LayerName::Landlock, state);
                let files = Files {
                    outcome: skipped.clone(),
                    abi: None,
                };
                assert_eq!(cover(layer, files, seccomp).unwrap(), skipped);
            }
        }
    }
}

#[test]
fn abi_5_and_later_need_no_cover() {
    for layer in [Layer::BestEffort, Layer::Required] {
        for seccomp in FILTERED.into_iter().chain(UNFILTERED) {
            for abi in [5, 6, 7] {
                assert_eq!(
                    cover(layer, landlock(abi), seccomp).unwrap(),
                    landlock(abi).outcome
                );
            }
        }
    }
}

#[test]
fn the_filter_covers_device_ioctl() {
    let active = |abi| State::Active(Applied::Abi(abi));
    for layer in [Layer::BestEffort, Layer::Required] {
        for seccomp in FILTERED {
            for abi in [3, 4] {
                assert_eq!(
                    cover(layer, landlock(abi), seccomp).unwrap(),
                    outcome(active(abi), &[Gap::DeviceIoctl], &[])
                );
            }
            // Truncation stays uncovered and is no failure, also under
            // `required` (owner decision 2026-11-01).
            for abi in [1, 2] {
                assert_eq!(
                    cover(layer, landlock(abi), seccomp).unwrap(),
                    outcome(active(abi), &[Gap::DeviceIoctl], &[Gap::Truncate])
                );
            }
        }
    }
}

#[test]
fn without_the_filter_device_ioctl_makes_landlock_partial() {
    let partial = |abi| State::Partial(Applied::Abi(abi));
    for seccomp in UNFILTERED {
        for abi in [3, 4] {
            assert_eq!(
                cover(Layer::BestEffort, landlock(abi), seccomp).unwrap(),
                outcome(partial(abi), &[], &[Gap::DeviceIoctl])
            );
        }
        for abi in [1, 2] {
            assert_eq!(
                cover(Layer::BestEffort, landlock(abi), seccomp).unwrap(),
                outcome(partial(abi), &[], &[Gap::Truncate, Gap::DeviceIoctl])
            );
        }
    }
}

#[test]
fn without_the_filter_device_ioctl_fails_a_required_landlock() {
    for (seccomp, word) in [(State::Off, "off"), (State::Missing, "missing")] {
        for abi in [1, 2, 3, 4] {
            assert_eq!(
                message(cover(Layer::Required, landlock(abi), seccomp)),
                format!(
                    "landlock is required but the kernel's Landlock (ABI {abi}) cannot \
                     restrict device_ioctl, and seccomp, which covers it, is {word}"
                )
            );
        }
    }
}
