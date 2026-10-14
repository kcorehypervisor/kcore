use kcore_verification::disk_layout::*;
use proptest::prelude::*;

fn device_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9]{0,8}".prop_map(|name| format!("/dev/{name}"))
}

fn role_strategy() -> impl Strategy<Value = DeviceRole> {
    prop_oneof![
        Just(DeviceRole::Free),
        Just(DeviceRole::ActiveVm),
        Just(DeviceRole::SystemMount),
        Just(DeviceRole::LvmPv),
        Just(DeviceRole::ZpoolMember),
    ]
}

proptest! {
    #[test]
    fn safe_verdict_never_overlaps_a_dangerous_role(
        path in device_name(),
        role in role_strategy()
    ) {
        let verdict = classify_device(&path, role);
        if verdict == Verdict::Safe {
            prop_assert!(!role.is_dangerous());
            prop_assert!(is_device_path(&path));
        }
        if role.is_dangerous() {
            prop_assert_ne!(verdict, Verdict::Safe);
        }
    }

    #[test]
    fn non_device_paths_are_refused(path in "[a-zA-Z0-9_./]{0,16}", role in role_strategy()) {
        prop_assume!(!path.starts_with("/dev/") || path == "/dev/" || path.contains('\0'));
        prop_assert_ne!(classify_device(&path, role), Verdict::Safe);
    }

    #[test]
    fn layout_is_safe_only_when_every_target_is(
        targets in prop::collection::vec((device_name(), role_strategy()), 0..=4)
    ) {
        let borrowed: Vec<(&str, DeviceRole)> = targets
            .iter()
            .map(|(path, role)| (path.as_str(), *role))
            .collect();
        let verdict = classify_layout(&borrowed);
        let safe = verdict == Verdict::Safe;
        if safe {
            for (path, role) in &targets {
                prop_assert!(!role.is_dangerous());
                prop_assert!(is_device_path(path));
            }
        } else {
            let any_unsafe = targets.iter().any(|(path, role)| {
                classify_device(path, *role) != Verdict::Safe
            });
            prop_assert!(any_unsafe);
        }
    }

    #[test]
    fn comment_strip_preserves_byte_length(input in "\\PC{0,48}") {
        let stripped = strip_nix_comments(&input);
        prop_assert_eq!(stripped.len(), input.len());
        prop_assert_eq!(strip_nix_comments(&stripped), stripped);
    }

    #[test]
    fn extracted_paths_are_device_paths(name in "[a-z0-9]{1,6}", comment in "[a-z0-9]{0,6}") {
        let config = format!("disk = \"/dev/{name}\"; # /dev/{comment}\n");
        let devices = extract_target_devices(&config);
        let expected = format!("/dev/{name}");
        let commented = format!("/dev/{comment}");
        prop_assert!(devices.iter().all(|path| is_device_path(path)));
        prop_assert!(devices.contains(&expected));
        prop_assume!(comment != name);
        if !comment.is_empty() {
            prop_assert!(!devices.contains(&commented));
        }
    }

    #[test]
    fn extraction_is_deterministic(input in "\\PC{0,40}") {
        prop_assert_eq!(extract_target_devices(&input), extract_target_devices(&input));
    }
}
