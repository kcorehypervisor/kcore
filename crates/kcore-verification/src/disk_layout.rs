//! Disk-layout safety checks shared by the controller pre-flight and the
//! node agent.
//!
//! A layout is safe only when every target is a `/dev/` path and none of
//! those devices currently back an active VM, a system mount, an LVM
//! physical volume, or a ZFS pool member.

/// Why a device must not be repartitioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRole {
    Free,
    ActiveVm,
    SystemMount,
    LvmPv,
    ZpoolMember,
}

impl DeviceRole {
    pub fn is_dangerous(self) -> bool {
        !matches!(self, DeviceRole::Free)
    }
}

/// Why a layout was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    EmptyPath,
    NotDevicePath,
    ActiveVm,
    SystemMount,
    LvmPv,
    ZpoolMember,
}

/// Classifier result. `Safe` means the layout touches nothing dangerous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Safe,
    Refused(RefusalReason),
}

fn refusal_for(role: DeviceRole) -> Option<RefusalReason> {
    match role {
        DeviceRole::Free => None,
        DeviceRole::ActiveVm => Some(RefusalReason::ActiveVm),
        DeviceRole::SystemMount => Some(RefusalReason::SystemMount),
        DeviceRole::LvmPv => Some(RefusalReason::LvmPv),
        DeviceRole::ZpoolMember => Some(RefusalReason::ZpoolMember),
    }
}

/// Classify one target device.
pub fn classify_device(path: &str, role: DeviceRole) -> Verdict {
    if path.is_empty() {
        return Verdict::Refused(RefusalReason::EmptyPath);
    }
    if !is_device_path(path) {
        return Verdict::Refused(RefusalReason::NotDevicePath);
    }
    match refusal_for(role) {
        Some(reason) => Verdict::Refused(reason),
        None => Verdict::Safe,
    }
}

/// Classify a whole layout. An empty target list touches no disk and is safe.
/// The first dangerous target refuses the layout.
pub fn classify_layout(targets: &[(&str, DeviceRole)]) -> Verdict {
    for (path, role) in targets {
        let verdict = classify_device(path, *role);
        if verdict != Verdict::Safe {
            return verdict;
        }
    }
    Verdict::Safe
}

/// True for concrete `/dev/...` paths. `/dev/` alone is not a device.
pub fn is_device_path(path: &str) -> bool {
    path.starts_with("/dev/") && path.len() > "/dev/".len() && !path.contains('\0')
}

/// Replace `#` comments with spaces without changing the byte length.
///
/// Comment bytes are blanked, so a `/dev/` path that appears only inside a
/// comment cannot be extracted afterwards. The result stays valid UTF-8
/// because every replaced byte becomes ASCII space and the uncommented
/// prefix and suffix were already valid UTF-8.
pub fn strip_nix_comments(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = vec![0u8; bytes.len()];
    let mut in_comment = false;
    for (index, byte) in bytes.iter().enumerate() {
        if in_comment {
            if *byte == b'\n' {
                in_comment = false;
                out[index] = b'\n';
            } else {
                out[index] = b' ';
            }
        } else if *byte == b'#' {
            in_comment = true;
            out[index] = b' ';
        } else {
            out[index] = *byte;
        }
    }
    String::from_utf8(out)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

fn is_device_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'.')
}

/// Pull `/dev/...` tokens out of a disko snippet.
///
/// Comment-stripped first, so commented devices are ignored. Every returned
/// path starts with `/dev/` and names a device.
pub fn extract_target_devices(config: &str) -> Vec<String> {
    let stripped = strip_nix_comments(config);
    let bytes = stripped.as_bytes();
    let mut devices = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if index + 5 <= bytes.len() && &bytes[index..index + 5] == b"/dev/" {
            let start = index;
            index += 5;
            while index < bytes.len() && is_device_token_byte(bytes[index]) {
                index += 1;
            }
            if index > start + 5 {
                if let Ok(path) = std::str::from_utf8(&bytes[start..index]) {
                    devices.push(path.to_string());
                }
            }
        } else {
            index += 1;
        }
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_only_for_free_device_paths() {
        assert_eq!(classify_device("/dev/sdb", DeviceRole::Free), Verdict::Safe);
        assert_eq!(
            classify_device("/dev/sda", DeviceRole::ActiveVm),
            Verdict::Refused(RefusalReason::ActiveVm)
        );
        assert_eq!(
            classify_device("/dev/nvme0n1", DeviceRole::SystemMount),
            Verdict::Refused(RefusalReason::SystemMount)
        );
        assert_eq!(
            classify_device("sdb", DeviceRole::Free),
            Verdict::Refused(RefusalReason::NotDevicePath)
        );
        assert_eq!(
            classify_device("", DeviceRole::Free),
            Verdict::Refused(RefusalReason::EmptyPath)
        );
    }

    #[test]
    fn layout_refuses_on_first_dangerous_disk() {
        let targets = [
            ("/dev/sdb", DeviceRole::Free),
            ("/dev/sdc", DeviceRole::LvmPv),
            ("/dev/sdd", DeviceRole::Free),
        ];
        assert_eq!(
            classify_layout(&targets),
            Verdict::Refused(RefusalReason::LvmPv)
        );
        assert_eq!(
            classify_layout(&[("/dev/sdb", DeviceRole::Free)]),
            Verdict::Safe
        );
        assert_eq!(classify_layout(&[]), Verdict::Safe);
    }

    #[test]
    fn comment_strip_preserves_length_and_hides_devices() {
        let config = "device = \"/dev/sda\"; # keep /dev/sdb\n";
        let stripped = strip_nix_comments(config);
        assert_eq!(stripped.len(), config.len());
        assert!(!stripped.contains("/dev/sdb"));
        assert_eq!(extract_target_devices(config), vec!["/dev/sda".to_string()]);
    }

    #[test]
    fn extract_is_deterministic_and_dev_prefixed() {
        let config = "disks = [ \"/dev/nvme0n1\" \"/dev/vda\" ];";
        let once = extract_target_devices(config);
        let twice = extract_target_devices(config);
        assert_eq!(once, twice);
        assert!(once
            .iter()
            .all(|path| path.starts_with("/dev/") && path.len() > 5));
        assert_eq!(
            once,
            vec!["/dev/nvme0n1".to_string(), "/dev/vda".to_string()]
        );
    }
}
