//! One-shot rehearsal of an LVM disk move, used by the NixOS test.
//!
//! Two machines share the commands the node agent runs: the destination
//! exports a new LV over NBD, the source mirrors onto it, and both sides
//! clean up. Cloud Hypervisor memory migration is not part of this rehearsal;
//! the disk copy is the part that has no other host.

use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::{
    abort_session, copy_quiescent, destroy_source_copy, dm_status, linear_table, lvcreate_argv,
    mapper_path, mirror_status, prepare_target, release_target_export, start_mirror, DmStatus,
    NbdTarget,
};

const VM: &str = "itmig";
const SRC_VG: &str = "migsrc";
const DST_VG: &str = "migdst";
const SOCKET_DIR: &str = "/run/kcore-mirror";
/// 32 MiB. One power-of-two mirror region is 16 MiB, so this is two regions
/// and still finishes inside a test VM.
const VOLUME_BYTES: u64 = 32 * 1024 * 1024;

pub fn run(args: &[String]) -> Result<(), String> {
    let cmd = args.get(1).map(String::as_str);
    match cmd {
        Some("setup-source") => setup_source(need(args, 2, "DISK")?),
        Some("prepare-target") => prepare_dest(
            need(args, 2, "DISK")?,
            need(args, 3, "BIND")?,
            need(args, 4, "BYTES")?,
        ),
        Some("sync-source") => sync_source(
            need(args, 2, "HOST")?,
            need(args, 3, "PORT")?,
            need(args, 4, "EXPORT")?,
        ),
        Some("expect-dest") => expect_dest(&[
            (0, parse_byte(need(args, 2, "B0")?)?),
            (4096, parse_byte(need(args, 3, "B4096")?)?),
            (8192, parse_byte(need(args, 4, "B8192")?)?),
        ]),
        Some("abort-source") => abort_source(),
        Some("abort-target") => abort_target(),
        Some("cold-copy") => cold_copy(
            need(args, 2, "HOST")?,
            need(args, 3, "PORT")?,
            need(args, 4, "EXPORT")?,
        ),
        Some("release-target") => release_target(),
        Some("reject-source-delete") => reject_source_delete(),
        Some("destroy-source") => destroy_source(),
        Some("cleanup-source") => cleanup(SRC_VG, need(args, 2, "DISK")?),
        Some("cleanup-dest") => cleanup(DST_VG, need(args, 2, "DISK")?),
        Some(other) => Err(format!("unknown rehearsal command {other}")),
        None => Err(
            "usage: kcore-volume-mirror-check setup-source|prepare-target|sync-source|...".into(),
        ),
    }
}

fn need<'a>(args: &'a [String], index: usize, name: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("missing {name}"))
}

fn parse_byte(text: &str) -> Result<u8, String> {
    u8::from_str_radix(text, 16).map_err(|_| format!("byte '{text}' is not hex"))
}

fn socket_dir() -> Result<&'static Path, String> {
    let dir = Path::new(SOCKET_DIR);
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir)
}

fn sh(argv: &[String]) -> Result<(), String> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    let out = cmd
        .output()
        .map_err(|e| format!("spawn {}: {e}", argv[0]))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    Err(format!(
        "{} exited {}: {stderr}{stdout}",
        argv.join(" "),
        out.status
    ))
}

fn capture(argv: &[String]) -> Result<String, String> {
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| format!("spawn {}: {e}", argv[0]))?;
    if !out.status.success() {
        return Err(format!(
            "{} exited {}: {}",
            argv.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn modprobe_mirror() -> Result<(), String> {
    let _ = sh(&["modprobe".into(), "dm-mod".into()]);
    let _ = sh(&["modprobe".into(), "dm-mirror".into()]);
    let _ = sh(&["modprobe".into(), "nbd".into(), "nbds_max=16".into()]);
    Ok(())
}

fn ensure_vg(disk: &str, vg: &str) -> Result<(), String> {
    if Path::new(&format!("/dev/{vg}")).exists() {
        return Ok(());
    }
    let _ = sh(&["wipefs".into(), "-a".into(), disk.into()]);
    sh(&["pvcreate".into(), "-ff".into(), "-y".into(), disk.into()])?;
    sh(&["vgcreate".into(), "-y".into(), vg.into(), disk.into()])
}

fn setup_source(disk: &str) -> Result<(), String> {
    modprobe_mirror()?;
    ensure_vg(disk, SRC_VG)?;
    let backing = format!("/dev/{SRC_VG}/kcore-{VM}");
    if !Path::new(&backing).exists() {
        sh(&lvcreate_argv(SRC_VG, VM, VOLUME_BYTES)?)?;
    }
    let sectors: u64 = capture(&["blockdev".into(), "--getsz".into(), backing.clone()])?
        .parse()
        .map_err(|e| format!("source sector count: {e}"))?;
    let mapper = mapper_path(VM)?;
    if !Path::new(&mapper).exists() {
        sh_stdin(
            &["dmsetup".into(), "create".into(), format!("kcore-vm-{VM}")],
            &linear_table(sectors, &backing)?,
        )?;
    }
    write_sector(&mapper, 0, 0xA5)?;
    write_sector(&mapper, 16, 0x5A)?;
    let bytes = capture(&["blockdev".into(), "--getsize64".into(), mapper])?;
    println!("{bytes}");
    Ok(())
}

fn prepare_dest(disk: &str, bind: &str, bytes: &str) -> Result<(), String> {
    modprobe_mirror()?;
    ensure_vg(disk, DST_VG)?;
    let size: u64 = bytes
        .parse()
        .map_err(|_| format!("volume size '{bytes}' is not a number"))?;
    let bind_addr = bind
        .parse()
        .map_err(|_| format!("bind address '{bind}' is not an IP"))?;
    let session = prepare_target(socket_dir()?, VM, "lvm", DST_VG, "", size, bind_addr)?;
    println!("{} {}", session.port, session.export_name);
    Ok(())
}

fn sync_source(host: &str, port: &str, export: &str) -> Result<(), String> {
    let port: u16 = port
        .parse()
        .map_err(|_| format!("port '{port}' is not a number"))?;
    start_mirror(
        socket_dir()?,
        VM,
        "lvm",
        SRC_VG,
        "",
        NbdTarget { host, port, export },
    )?;
    let deadline = Instant::now() + Duration::from_secs(90);
    let raw = loop {
        let progress = mirror_status(VM)?;
        let raw = capture(&["dmsetup".into(), "status".into(), format!("kcore-vm-{VM}")])?;
        if progress.degraded {
            return Err(format!("mirror went degraded: {raw}"));
        }
        if progress.in_sync {
            if !raw.contains(" 1 AA") {
                return Err(format!(
                    "mirror is in sync but the kernel status line has no '1 AA' health field: {raw}"
                ));
            }
            break raw;
        }
        if Instant::now() >= deadline {
            return Err(format!("mirror did not reach in-sync within 90s: {raw}"));
        }
        thread::sleep(Duration::from_millis(200));
    };
    // A write after the resync must land on the destination leg as well.
    write_sector(&mapper_path(VM)?, 8, 0x3C)?;
    println!("{raw}");
    Ok(())
}

fn expect_dest(want: &[(u64, u8)]) -> Result<(), String> {
    let path = format!("/dev/{DST_VG}/kcore-{VM}");
    for (offset, byte) in want {
        let got = read_byte(&path, *offset)?;
        if got != *byte {
            return Err(format!(
                "{path} offset {offset}: expected {byte:02x}, read {got:02x}"
            ));
        }
    }
    println!("destination bytes matched");
    Ok(())
}

fn abort_source() -> Result<(), String> {
    let message = abort_session(socket_dir()?, VM, false)?;
    let status = dm_status(VM)?;
    if !matches!(status, DmStatus::Linear { .. }) {
        return Err(format!("source is not linear after abort: {status:?}"));
    }
    println!("{message}");
    Ok(())
}

fn abort_target() -> Result<(), String> {
    let message = abort_session(socket_dir()?, VM, true)?;
    let path = format!("/dev/{DST_VG}/kcore-{VM}");
    if Path::new(&path).exists() {
        return Err(format!("{path} still exists after the target abort"));
    }
    println!("{message}");
    Ok(())
}

fn cold_copy(host: &str, port: &str, export: &str) -> Result<(), String> {
    let port: u16 = port
        .parse()
        .map_err(|_| format!("port '{port}' is not a number"))?;
    copy_quiescent(
        socket_dir()?,
        VM,
        "lvm",
        SRC_VG,
        "",
        NbdTarget { host, port, export },
    )?;
    println!("cold copy finished");
    Ok(())
}

fn release_target() -> Result<(), String> {
    release_target_export(socket_dir()?, VM)?;
    if qemu_nbd_running()? {
        return Err("qemu-nbd is still running after the target export was released".into());
    }
    println!("target export released");
    Ok(())
}

fn reject_source_delete() -> Result<(), String> {
    match destroy_source_copy(socket_dir()?, VM) {
        Err(err) if err.contains("mirror target") => {
            println!("refused to delete the destination volume");
            Ok(())
        }
        Ok(()) => Err("destroy_source_copy removed the destination volume".into()),
        Err(err) => Err(err),
    }
}

fn destroy_source() -> Result<(), String> {
    destroy_source_copy(socket_dir()?, VM)?;
    let backing = format!("/dev/{SRC_VG}/kcore-{VM}");
    if Path::new(&backing).exists() {
        return Err(format!(
            "{backing} still exists after the source copy was dropped"
        ));
    }
    println!("source copy removed");
    Ok(())
}

fn cleanup(vg: &str, disk: &str) -> Result<(), String> {
    let _ = sh(&["dmsetup".into(), "remove".into(), format!("kcore-vm-{VM}")]);
    let _ = sh(&["lvremove".into(), "-ff".into(), "-y".into(), vg.into()]);
    let _ = sh(&["vgremove".into(), "-ff".into(), "-y".into(), vg.into()]);
    let _ = sh(&["pvremove".into(), "-ff".into(), "-y".into(), disk.into()]);
    Ok(())
}

fn qemu_nbd_running() -> Result<bool, String> {
    let out = Command::new("pgrep")
        .args(["-x", "qemu-nbd"])
        .output()
        .map_err(|e| format!("spawn pgrep: {e}"))?;
    Ok(out.status.success())
}

fn write_sector(path: &str, sector: u64, byte: u8) -> Result<(), String> {
    let mut child = Command::new("dd")
        .args([
            format!("of={path}"),
            "bs=512".into(),
            format!("seek={sector}"),
            "count=1".into(),
            "oflag=direct".into(),
            "conv=notrunc".into(),
            "status=none".into(),
        ])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn dd: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(&[byte; 512])
            .map_err(|e| format!("write pattern to {path}: {e}"))?;
    }
    let status = child.wait().map_err(|e| format!("wait dd: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "dd write to {path} sector {sector} exited {status}"
        ))
    }
}

fn read_byte(path: &str, offset: u64) -> Result<u8, String> {
    let sector = offset / 512;
    let within = (offset % 512) as usize;
    let out = Command::new("dd")
        .args([
            format!("if={path}"),
            "bs=512".into(),
            format!("skip={sector}"),
            "count=1".into(),
            "iflag=direct".into(),
            "status=none".into(),
        ])
        .output()
        .map_err(|e| format!("spawn dd: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "dd read {path}: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    out.stdout
        .get(within)
        .copied()
        .ok_or_else(|| format!("short read from {path} at {offset}"))
}

fn sh_stdin(argv: &[String], stdin_text: &str) -> Result<(), String> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", argv[0]))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(stdin_text.as_bytes())
            .map_err(|e| format!("write stdin to {}: {e}", argv[0]))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait {}: {e}", argv[0]))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{} exited {}: {}",
            argv.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}
