//! Tests for the `containment` block (FR-CFG-25).

use super::super::super::model::{Layer, SeccompAction};
use super::*;

/// A valid file with `inner` as the body of a `containment` block starting
/// on line 2 (its first setting is at 3:3).
fn with_containment(inner: &str) -> String {
    cfg(&format!(
        "containment {{\n  {inner}\n}}\nhost a {{ path / {{ static /s; }} }}\n"
    ))
}

fn no_effect(key: &str, with: &str) -> ConfigErrorKind {
    ConfigErrorKind::NoEffect {
        key: key.to_string(),
        with: with.to_string(),
    }
}

#[test]
fn no_effect_error_reads_clearly() {
    let e = load(&with_containment("seccomp off;\n  seccomp_action kill;"))
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        e.as_deref(),
        Some("4:3: `seccomp_action` has no effect with `seccomp off`")
    );
}

#[test]
fn defaults_without_the_block() {
    let config = ok(&cfg("host a { path / { static /s; } }\n"));
    assert_eq!(
        config.containment,
        Containment {
            landlock: Layer::BestEffort,
            seccomp: Layer::BestEffort,
            seccomp_action: SeccompAction::Fail,
        }
    );
    assert_eq!(config.containment, Containment::default());
    assert_eq!(config.limits.ipv6_prefix, 64);
}

#[test]
fn empty_block_is_the_defaults() {
    assert_eq!(
        ok(&with_containment("")).containment,
        Containment::default()
    );
}

#[test]
fn layers_and_action_parse() {
    let config = ok(&with_containment("landlock required;\n  seccomp off;"));
    assert_eq!(
        config.containment,
        Containment {
            landlock: Layer::Required,
            seccomp: Layer::Off,
            seccomp_action: SeccompAction::Fail,
        }
    );
    let config = ok(&with_containment(
        "landlock best_effort;\n  seccomp required;",
    ));
    assert_eq!(config.containment.landlock, Layer::BestEffort);
    assert_eq!(config.containment.seccomp, Layer::Required);
    let config = ok(&with_containment("landlock off;"));
    assert_eq!(config.containment.landlock, Layer::Off);
}

#[test]
fn seccomp_action_parses() {
    let config = ok(&with_containment("seccomp_action kill;"));
    assert_eq!(config.containment.seccomp_action, SeccompAction::Kill);
    assert_eq!(config.containment.seccomp, Layer::BestEffort);
    let config = ok(&with_containment(
        "seccomp required;\n  seccomp_action fail;",
    ));
    assert_eq!(config.containment.seccomp_action, SeccompAction::Fail);
    // Written before `seccomp`, with `seccomp` not off.
    let config = ok(&with_containment(
        "seccomp_action kill;\n  seccomp required;",
    ));
    assert_eq!(config.containment.seccomp_action, SeccompAction::Kill);
}

/// FR-CFG-25 (builder call): a `seccomp_action` that does nothing is an
/// error, at the `seccomp_action` setting, in either order.
#[test]
fn seccomp_action_with_seccomp_off_is_an_error() {
    assert_eq!(
        err(&with_containment("seccomp off;\n  seccomp_action kill;")),
        (no_effect("seccomp_action", "seccomp off"), 4, 3)
    );
    assert_eq!(
        err(&with_containment("seccomp_action fail;\n  seccomp off;")),
        (no_effect("seccomp_action", "seccomp off"), 3, 3)
    );
}

#[test]
fn bad_values_keys_and_counts() {
    assert_eq!(
        err(&with_containment("landlock maybe;")),
        (invalid("landlock", "maybe"), 3, 3)
    );
    assert_eq!(
        err(&with_containment("seccomp Required;")),
        (invalid("seccomp", "Required"), 3, 3)
    );
    assert_eq!(
        err(&with_containment("seccomp_action die;")),
        (invalid("seccomp_action", "die"), 3, 3)
    );
    // `kill` is not a layer, `required` is not an action.
    assert_eq!(
        err(&with_containment("landlock kill;")),
        (invalid("landlock", "kill"), 3, 3)
    );
    assert_eq!(
        err(&with_containment("seccomp_action required;")),
        (invalid("seccomp_action", "required"), 3, 3)
    );
    let count = |key: &str| ConfigErrorKind::WrongValueCount(key.to_string());
    assert_eq!(
        err(&with_containment("landlock;")),
        (count("landlock"), 3, 3)
    );
    assert_eq!(
        err(&with_containment("seccomp off off;")),
        (count("seccomp"), 3, 3)
    );
    assert_eq!(
        err(&with_containment("sandbox off;")),
        (ConfigErrorKind::UnknownKey("sandbox".to_string()), 3, 3)
    );
    assert_eq!(
        err(&with_containment("landlock off { }")),
        (ConfigErrorKind::UnknownKey("landlock".to_string()), 3, 3)
    );
    assert_eq!(
        err(&with_containment("inner { }")),
        (ConfigErrorKind::UnknownKey("inner".to_string()), 3, 3)
    );
}

#[test]
fn duplicate_keys_and_blocks() {
    for key in ["landlock off", "seccomp off", "seccomp_action kill"] {
        let name = key.split(' ').next().unwrap_or_default();
        let inner = if name == "seccomp_action" {
            format!("{key};\n  {key};")
        } else {
            format!("{key};\n  {name} required;")
        };
        assert_eq!(
            err(&with_containment(&inner)),
            (ConfigErrorKind::DuplicateKey(name.to_string()), 4, 3),
            "{key}"
        );
    }
    assert_eq!(
        err(&cfg("containment { }\ncontainment { }\n")),
        (
            ConfigErrorKind::DuplicateKey("containment".to_string()),
            3,
            1
        )
    );
}

#[test]
fn block_shape_and_placement() {
    assert_eq!(
        err(&cfg("containment 5;\n")),
        (ConfigErrorKind::UnknownKey("containment".to_string()), 2, 1)
    );
    assert_eq!(
        err(&cfg("containment x { }\n")),
        (
            ConfigErrorKind::WrongValueCount("containment".to_string()),
            2,
            1
        )
    );
    // Top level only: not inside a host, a path or limits.
    assert_eq!(
        err(&in_host("  containment { }")),
        (ConfigErrorKind::UnknownKey("containment".to_string()), 3, 3)
    );
    assert_eq!(
        err(&in_host("  path / { containment { } }")),
        (
            ConfigErrorKind::UnknownKey("containment".to_string()),
            3,
            12
        )
    );
    assert_eq!(
        err(&with_limit("containment { }")),
        (ConfigErrorKind::UnknownKey("containment".to_string()), 3, 3)
    );
}

/// The block's keys are documented in the example config, from one list.
#[test]
fn containment_key_list_is_complete() {
    fn index(key: ContainmentKey) -> usize {
        match key {
            ContainmentKey::Landlock => 0,
            ContainmentKey::Seccomp => 1,
            ContainmentKey::SeccompAction => 2,
        }
    }
    let mut seen = [false; ContainmentKey::ALL.len()];
    for (name, key) in ContainmentKey::ALL {
        assert_eq!(ContainmentKey::from_name(name), Some(key), "{name}");
        let slot = seen.get_mut(index(key));
        assert_eq!(slot.as_deref(), Some(&false), "{name} listed twice");
        if let Some(slot) = slot {
            *slot = true;
        }
    }
    assert!(seen.iter().all(|s| *s), "a key is missing from ALL");
}
