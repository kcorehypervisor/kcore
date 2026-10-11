//! Host CPU facts that decide whether a live migration can land here.
//!
//! Cloud Hypervisor compares CPUID feature bits itself. What it does not
//! surface is the TSC rate: without hardware TSC scaling KVM accepts a guest
//! TSC above the host rate (catch-up mode) but rejects one below it, so a
//! 3.3 GHz -> 3.4 GHz move used to abort at restore with no usable error.

use std::fmt;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostCpu {
    pub vendor: String,
    pub model_name: String,
    /// Nominal TSC rate (CPUID leaf 0x16 base frequency); 0 = unknown.
    pub tsc_khz: u32,
    pub tsc_scaling: bool,
}

impl fmt::Display for HostCpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}), TSC {} kHz, TSC scaling {}",
            self.model_name.trim(),
            self.vendor,
            self.tsc_khz,
            if self.tsc_scaling { "yes" } else { "no" }
        )
    }
}

/// What the destination should do with a source/destination pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Compat {
    Ok,
    Warn(Vec<String>),
    Refuse(String),
}

pub fn local() -> HostCpu {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    from_cpuinfo(&cpuinfo, cpuid_base_khz())
}

/// Parse the first processor's entries. `tsc_khz` comes from CPUID, not the
/// text, because "cpu MHz" there is the current (scaled) frequency.
pub fn from_cpuinfo(cpuinfo: &str, tsc_khz: u32) -> HostCpu {
    let mut cpu = HostCpu {
        tsc_khz,
        ..Default::default()
    };
    for line in cpuinfo.lines() {
        let Some((key, value)) = line.split_once(':') else {
            // Blank line ends the first processor block.
            if line.trim().is_empty() && !cpu.model_name.is_empty() {
                break;
            }
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "vendor_id" if cpu.vendor.is_empty() => cpu.vendor = value.to_string(),
            "model name" if cpu.model_name.is_empty() => cpu.model_name = value.to_string(),
            // Intel lists VMX capabilities on their own line; AMD puts the
            // SVM TSC ratio feature in the main flags line.
            "vmx flags" => {
                if value.split_whitespace().any(|f| f == "tsc_scaling") {
                    cpu.tsc_scaling = true;
                }
            }
            "flags" => {
                if value
                    .split_whitespace()
                    .any(|f| f == "tsc_scale" || f == "tsc_scaling")
                {
                    cpu.tsc_scaling = true;
                }
            }
            _ => {}
        }
    }
    cpu
}

#[cfg(target_arch = "x86_64")]
fn cpuid_base_khz() -> u32 {
    // Leaf 0 gates the maximum leaf before 0x16 is queried.
    let max_leaf = std::arch::x86_64::__cpuid(0).eax;
    if max_leaf < 0x16 {
        return 0;
    }
    let base_mhz = std::arch::x86_64::__cpuid(0x16).eax & 0xffff;
    base_mhz.saturating_mul(1000)
}

#[cfg(not(target_arch = "x86_64"))]
fn cpuid_base_khz() -> u32 {
    0
}

/// Decide for a guest leaving `source` and arriving on `dest`.
pub fn check(source: &HostCpu, dest: &HostCpu) -> Compat {
    if !source.vendor.is_empty() && !dest.vendor.is_empty() && source.vendor != dest.vendor {
        return Compat::Refuse(format!(
            "CPU vendor differs: source {} vs destination {}; Cloud Hypervisor cannot \
             restore the vCPU state across vendors",
            source.vendor, dest.vendor
        ));
    }
    let mut warnings = Vec::new();
    if source.tsc_khz != 0 && dest.tsc_khz != 0 && source.tsc_khz != dest.tsc_khz {
        if dest.tsc_scaling {
            warnings.push(format!(
                "TSC rate differs (source {} kHz, destination {} kHz); the destination \
                 scales the guest TSC in hardware",
                source.tsc_khz, dest.tsc_khz
            ));
        } else if source.tsc_khz > dest.tsc_khz {
            warnings.push(format!(
                "TSC rate differs (source {} kHz, destination {} kHz) and the destination \
                 has no TSC scaling; KVM runs the guest TSC in catch-up mode",
                source.tsc_khz, dest.tsc_khz
            ));
        } else {
            warnings.push(format!(
                "TSC rate differs (source {} kHz, destination {} kHz) and the destination \
                 has no TSC scaling; the guest TSC keeps the destination rate (guests on \
                 kvm-clock or the Hyper-V reference TSC page keep correct time). Needs the \
                 kcore Cloud Hypervisor TSC patch on the destination.",
                source.tsc_khz, dest.tsc_khz
            ));
        }
    }
    if !source.model_name.is_empty()
        && !dest.model_name.is_empty()
        && source.model_name.trim() != dest.model_name.trim()
    {
        warnings.push(format!(
            "CPU model differs: source '{}' vs destination '{}'",
            source.model_name.trim(),
            dest.model_name.trim()
        ));
    }
    if warnings.is_empty() {
        Compat::Ok
    } else {
        Compat::Warn(warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTEL: &str = "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Intel(R) Xeon(R) E-2124 CPU @ 3.30GHz\nflags\t\t: fpu vme tsc_adjust\nvmx flags\t: vnmi preemption_timer invvpid ept_x_only\n\nprocessor\t: 1\nvendor_id\t: GenuineIntel\nmodel name\t: other\n";
    const INTEL_SCALING: &str = "vendor_id\t: GenuineIntel\nmodel name\t: Intel(R) Xeon(R) Gold 6338\nflags\t\t: fpu\nvmx flags\t: vnmi tsc_scaling ept\n";
    const AMD: &str =
        "vendor_id\t: AuthenticAMD\nmodel name\t: AMD EPYC 7302P\nflags\t\t: fpu svm tsc_scale\n";

    #[test]
    fn parses_first_processor_only() {
        let cpu = from_cpuinfo(INTEL, 3_300_000);
        assert_eq!(cpu.vendor, "GenuineIntel");
        assert_eq!(cpu.model_name, "Intel(R) Xeon(R) E-2124 CPU @ 3.30GHz");
        assert_eq!(cpu.tsc_khz, 3_300_000);
        assert!(!cpu.tsc_scaling);
    }

    #[test]
    fn detects_tsc_scaling_on_both_vendors() {
        assert!(from_cpuinfo(INTEL_SCALING, 0).tsc_scaling);
        assert!(from_cpuinfo(AMD, 0).tsc_scaling);
    }

    #[test]
    fn vendor_mismatch_refuses() {
        let intel = from_cpuinfo(INTEL, 3_300_000);
        let amd = from_cpuinfo(AMD, 3_000_000);
        assert!(matches!(check(&intel, &amd), Compat::Refuse(_)));
    }

    #[test]
    fn same_host_is_ok() {
        let a = from_cpuinfo(INTEL, 3_300_000);
        assert_eq!(check(&a, &a.clone()), Compat::Ok);
    }

    #[test]
    fn slower_source_without_scaling_warns_about_patch() {
        let src = from_cpuinfo(INTEL, 3_300_000);
        let mut dst = from_cpuinfo(INTEL, 3_400_000);
        dst.model_name = "Intel(R) Xeon(R) E-2224 CPU @ 3.40GHz".into();
        match check(&src, &dst) {
            Compat::Warn(w) => {
                assert_eq!(w.len(), 2, "{w:?}");
                assert!(w[0].contains("TSC patch"), "{w:?}");
                assert!(w[1].contains("CPU model differs"), "{w:?}");
            }
            other => panic!("expected warnings, got {other:?}"),
        }
    }

    #[test]
    fn faster_source_without_scaling_is_catch_up() {
        let src = from_cpuinfo(INTEL, 3_400_000);
        let dst = from_cpuinfo(INTEL, 3_300_000);
        match check(&src, &dst) {
            Compat::Warn(w) => assert!(w[0].contains("catch-up"), "{w:?}"),
            other => panic!("expected warning, got {other:?}"),
        }
    }

    #[test]
    fn unknown_tsc_is_not_compared() {
        let src = from_cpuinfo(INTEL, 0);
        let dst = from_cpuinfo(INTEL, 3_400_000);
        assert_eq!(check(&src, &dst), Compat::Ok);
    }

    #[test]
    fn display_is_one_line() {
        let cpu = from_cpuinfo(INTEL, 3_300_000);
        let s = cpu.to_string();
        assert!(s.contains("E-2124") && s.contains("3300000 kHz") && !s.contains('\n'));
    }
}
