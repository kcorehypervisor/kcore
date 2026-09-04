//! `kctl` subcommands for inspecting and clearing live-migrate receive
//! sessions.
//!
//! A receive session is the destination-side state a node holds between
//! `PrepareLiveMigrateReceive` and the migration finishing. If the controller
//! dies in that window the session outlives the migration, and every later
//! attempt to migrate that VM to the same node is refused with
//! `ALREADY_EXISTS`. There is deliberately no automatic reaping — deciding a
//! session is dead from the outside risks tearing down a receive that is
//! actually in flight — so an operator inspects and then clears it.

use anyhow::{bail, Context, Result};

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

fn yes_no(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}

/// How a state reads to someone deciding whether clearing is safe.
fn verdict(state: &controller_proto::LiveMigrateReceiveState) -> &'static str {
    if (state.vmm_alive && state.vmm_pid_matches_vm) || state.port_listening {
        "LIVE - a receive may be in flight; clearing would kill it"
    } else {
        "stranded - nothing is receiving"
    }
}

fn print_state(state: &controller_proto::LiveMigrateReceiveState) {
    println!("    session tracked:   {}", yes_no(state.has_session));
    if state.port != 0 {
        println!(
            "    port:              {} ({})",
            state.port,
            if state.port_listening {
                "listening"
            } else {
                "not listening"
            }
        );
    }
    if state.session_pid != 0 {
        println!("    session pid:       {}", state.session_pid);
    }
    if state.pid_file_present {
        println!("    pid file:          {}", state.pid_file_pid);
    }
    if state.session_pid != 0 || state.pid_file_pid != 0 {
        println!(
            "    receive VMM:       {}{}",
            if state.vmm_alive {
                "alive"
            } else {
                "not running"
            },
            if state.vmm_alive && !state.vmm_pid_matches_vm {
                " (pid belongs to another process; it will not be killed)"
            } else {
                ""
            }
        );
    }
    println!("    handoff marker:    {}", yes_no(state.marker_present));
    println!(
        "    API socket:        {}",
        yes_no(state.api_socket_present)
    );
    println!("    verdict:           {}", verdict(state));
}

/// `kctl get migrate-session <vm> [--node ID]`
pub async fn status(info: &ConnectionInfo, vm: &str, node_id: Option<&str>) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let r = client
        .get_live_migrate_receive_status(controller_proto::GetLiveMigrateReceiveStatusRequest {
            vm_id: vm.to_string(),
            node_id: node_id.unwrap_or_default().to_string(),
        })
        .await
        .context("get_live_migrate_receive_status rpc")?
        .into_inner();

    println!("VM:            {} ({})", r.vm_name, r.vm_id);
    println!("Runtime name:  {}", r.runtime_name);
    println!("Current node:  {}", r.current_node);
    println!();

    if r.nodes.is_empty() {
        println!("No nodes to query. Live migrate only prepares a receive on CephCluster");
        println!("members, so pass --node explicitly if the node has since left the cluster.");
        return Ok(());
    }

    let mut sessions = 0;
    let mut live = 0;
    for node in &r.nodes {
        let owner = if node.owns_vm { " (owns this VM)" } else { "" };
        println!("Node {}{}", node.node_id, owner);
        if !node.reachable {
            println!("    unreachable:       {}", node.error);
            continue;
        }
        match &node.state {
            Some(state) => {
                print_state(state);
                if state.has_session {
                    sessions += 1;
                }
                if (state.vmm_alive && state.vmm_pid_matches_vm) || state.port_listening {
                    live += 1;
                }
            }
            None => println!("    no receive state reported"),
        }
        println!();
    }

    if sessions == 0 {
        println!("No prepared receive session anywhere. A migration failing with ALREADY_EXISTS");
        println!("is not caused by a stranded session.");
        return Ok(());
    }
    println!(
        "{sessions} node(s) hold a prepared receive session; {live} look like a live receive."
    );
    println!(
        "Clear a stranded one with `kctl migrate reset-session {} --node <id> --force`.",
        r.vm_name
    );
    if live > 0 {
        println!(
            "WARNING: at least one session looks LIVE. Clearing it will kill an in-flight \
             migration."
        );
    }
    Ok(())
}

/// The node whose session is being cleared must be named explicitly; there is
/// no "wherever you find one" mode.
fn require_node(node_id: &str) -> Result<&str> {
    let trimmed = node_id.trim();
    if trimmed.is_empty() {
        bail!("--node is required; name the node whose session you are clearing");
    }
    Ok(trimmed)
}

/// `kctl migrate reset-session <vm> --node ID [--force]`
///
/// Without `--force` this reports what it would clear and clears nothing, so
/// running it by accident costs an operator nothing.
pub async fn reset(info: &ConnectionInfo, vm: &str, node_id: &str, force: bool) -> Result<()> {
    let node_id = require_node(node_id)?;
    let mut client = client::controller_client(info).await?;
    let r = client
        .reset_live_migrate_receive(controller_proto::ResetLiveMigrateReceiveRequest {
            vm_id: vm.to_string(),
            node_id: node_id.to_string(),
            force,
        })
        .await
        .context("reset_live_migrate_receive rpc")?
        .into_inner();

    if let Some(state) = &r.observed {
        println!("Node {} before clearing:", r.node_id);
        print_state(state);
        println!();
    }
    println!("{}", r.message);

    if !r.success {
        bail!("reset failed: {}", r.message);
    }
    if !force && !r.cleared {
        // Nothing happened, so exiting zero would let `&&` chains carry on as
        // though the session were gone.
        bail!(
            "nothing was cleared. Re-run with --force once the state above shows the session is \
             not receiving"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> controller_proto::LiveMigrateReceiveState {
        controller_proto::LiveMigrateReceiveState::default()
    }

    #[test]
    fn verdict_calls_a_matching_live_pid_live() {
        let mut s = state();
        s.vmm_alive = true;
        s.vmm_pid_matches_vm = true;
        assert!(verdict(&s).starts_with("LIVE"));
    }

    /// A listening port is enough on its own: something owns the socket even
    /// if the pid bookkeeping was lost.
    #[test]
    fn verdict_calls_a_listening_port_live() {
        let mut s = state();
        s.port = 18000;
        s.port_listening = true;
        assert!(verdict(&s).starts_with("LIVE"));
    }

    /// The whole point of the pid-reuse check: an alive pid that is not our
    /// VMM must not be read as a live receive.
    #[test]
    fn verdict_treats_a_recycled_pid_as_stranded() {
        let mut s = state();
        s.vmm_alive = true;
        s.vmm_pid_matches_vm = false;
        assert_eq!(verdict(&s), "stranded - nothing is receiving");
    }

    #[test]
    fn verdict_treats_a_dead_session_as_stranded() {
        let mut s = state();
        s.has_session = true;
        s.port = 18000;
        s.pid_file_pid = 4242;
        assert_eq!(verdict(&s), "stranded - nothing is receiving");
    }

    #[test]
    fn require_node_rejects_blank_and_trims() {
        let err = require_node("   ").expect_err("blank node must be refused before dialling");
        assert!(err.to_string().contains("--node is required"));
        assert_eq!(require_node(" dell-1 ").expect("named node"), "dell-1");
    }
}
