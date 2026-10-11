//! kcore-disk-verify: prove a disk keeps its data across a power cycle.
//!
//! ```text
//! kcore-disk-verify write  --device /dev/sdb --nonce RUN --yes
//! (power cycle: ForceOff, then On)
//! kcore-disk-verify verify --device by-serial:BTWL4220020V300PGN --nonce RUN
//! ```
//!
//! Prints one JSON report on stdout. Exit status: 0 passed, 2 the disk failed
//! the check, 1 the tool could not run.

use anyhow::{bail, Context, Result};
use kcore_disk_verify::wipe::{wipe, WipeMode, WipeParams};
use kcore_disk_verify::{device, run_phase, Params, Phase, DEFAULT_REGIONS, DEFAULT_REGION_LEN};
use std::process::ExitCode;

const USAGE: &str = "\
Usage: kcore-disk-verify <write|verify> --device DEV --nonce NONCE [options]
       kcore-disk-verify wipe --device DEV --yes [--mode quick|full] [options]

  write   Destroys data on DEV. Needs --yes. DEV must not be mounted.
  verify  Read-only. Run it after a full power cycle, with the same NONCE.
  wipe    Destroys data on DEV. Needs --yes. quick zeroes the first and last
          32 MiB (default); full zeroes the whole disk. Reads the zeros back.
          No --nonce needed.

Options:
  --device DEV        /dev/... or by-serial:SERIAL
  --nonce NONCE       Run identifier, 1-64 of A-Z a-z 0-9 - _ (same for write and verify)
  --serial SERIAL     Drive serial as the BMC reports it (default: read from sysfs)
  --machine-id ID     Include in the JSON, ready for RecordDiskVerification
  --regions N         Regions to test (default 32)
  --region-kib N      Region size in KiB, multiple of 4 (default 1024)
  --mode MODE         wipe only: quick (default) or full
  --yes               Confirm that write or wipe destroys data
  --allow-file        Allow a regular file instead of a block device (testing)
  --no-direct         Do not use O_DIRECT (testing on tmpfs)
";

struct Cli {
    wipe: Option<WipeMode>,
    phase: Phase,
    device: String,
    nonce: String,
    serial: Option<String>,
    machine_id: Option<String>,
    regions: usize,
    region_len: u64,
    yes: bool,
    allow_file: bool,
    no_direct: bool,
}

fn parse_args(args: &[String]) -> Result<Cli> {
    let mut it = args.iter();
    let (phase, is_wipe) = match it.next().map(|s| s.as_str()) {
        Some("write") => (Phase::Write, false),
        Some("verify") => (Phase::Verify, false),
        Some("wipe") => (Phase::Write, true),
        _ => bail!("first argument must be write, verify or wipe"),
    };
    let mut wipe_mode = is_wipe.then_some(WipeMode::Quick);

    let mut cli = Cli {
        wipe: None,
        phase,
        device: String::new(),
        nonce: String::new(),
        serial: None,
        machine_id: None,
        regions: DEFAULT_REGIONS,
        region_len: DEFAULT_REGION_LEN,
        yes: false,
        allow_file: false,
        no_direct: false,
    };

    while let Some(arg) = it.next() {
        let mut value = |name: &str| -> Result<String> {
            it.next()
                .cloned()
                .with_context(|| format!("{} needs a value", name))
        };
        match arg.as_str() {
            "--device" => cli.device = value("--device")?,
            "--nonce" => cli.nonce = value("--nonce")?,
            "--serial" => cli.serial = Some(value("--serial")?),
            "--machine-id" => cli.machine_id = Some(value("--machine-id")?),
            "--regions" => cli.regions = value("--regions")?.parse().context("--regions")?,
            "--region-kib" => {
                let kib: u64 = value("--region-kib")?.parse().context("--region-kib")?;
                cli.region_len = kib.checked_mul(1024).context("--region-kib too large")?;
            }
            "--mode" => {
                if !is_wipe {
                    bail!("--mode only applies to wipe");
                }
                let m = value("--mode")?;
                wipe_mode = Some(WipeMode::parse(&m).context("--mode must be quick or full")?);
            }
            "--yes" => cli.yes = true,
            "--allow-file" => cli.allow_file = true,
            "--no-direct" => cli.no_direct = true,
            other => bail!("unknown argument {:?}", other),
        }
    }

    cli.wipe = wipe_mode;
    if cli.device.is_empty() {
        bail!("--device is required");
    }
    if is_wipe {
        return Ok(cli);
    }
    let nonce_ok = !cli.nonce.is_empty()
        && cli.nonce.len() <= 64
        && cli
            .nonce
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !nonce_ok {
        bail!("--nonce must be 1-64 characters of letters, digits, - or _");
    }
    if cli.regions == 0 || cli.regions > 4096 {
        bail!("--regions must be between 1 and 4096");
    }
    if cli.region_len == 0 || cli.region_len > 64 * 1024 * 1024 {
        bail!("--region-kib must be between 4 and 65536");
    }
    Ok(cli)
}

fn run(args: &[String]) -> Result<bool> {
    let cli = parse_args(args)?;

    let mut serial = cli.serial.clone();
    let device_path = if let Some(wanted) = cli.device.strip_prefix("by-serial:") {
        serial.get_or_insert_with(|| wanted.to_string());
        device::resolve_by_serial(wanted)?
    } else {
        cli.device.clone()
    };

    device::ensure_block_device(&device_path, cli.allow_file)?;

    if cli.phase == Phase::Write {
        if !cli.yes {
            bail!(
                "{} destroys data on {}; pass --yes to confirm",
                if cli.wipe.is_some() { "wipe" } else { "write" },
                device_path
            );
        }
        if !cli.allow_file {
            device::ensure_unused(&device_path)?;
        }
    }

    let serial = match serial {
        Some(s) => s,
        None => device::read_serial(&device_path)?,
    };

    let mut file = device::open(&device_path, cli.phase == Phase::Write, !cli.no_direct)?;
    if let Some(mode) = cli.wipe {
        let report = wipe(
            &mut file,
            &WipeParams {
                device: &device_path,
                serial: &serial,
                machine_id: cli.machine_id.as_deref(),
                mode,
            },
        )?;
        println!("{}", serde_json::to_string(&report)?);
        return Ok(report.passed);
    }
    let report = run_phase(
        &mut file,
        cli.phase,
        &Params {
            device: &device_path,
            serial: &serial,
            nonce: &cli.nonce,
            machine_id: cli.machine_id.as_deref(),
            region_len: cli.region_len,
            regions: cli.regions,
        },
    )?;

    println!("{}", serde_json::to_string(&report)?);
    Ok(report.passed)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprint!("{}", USAGE);
        return ExitCode::from(1);
    }

    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(e) => {
            eprintln!("error: {:#}", e);
            ExitCode::from(1)
        }
    }
}
