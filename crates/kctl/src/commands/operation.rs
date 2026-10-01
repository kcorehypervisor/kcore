use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;
use anyhow::{bail, Result};

pub async fn list(info: &ConnectionInfo, include_finished: bool, vm: Option<String>) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .list_vm_operations(controller_proto::ListVmOperationsRequest {
            include_finished,
            vm_id: vm.unwrap_or_default(),
        })
        .await?
        .into_inner();
    if resp.operations.is_empty() {
        println!("No operations found.");
        return Ok(());
    }
    println!(
        "{:<36}  {:<14}  {:<16}  {:<12}  {:<12}  VM",
        "ID", "KIND", "PHASE", "SOURCE", "TARGET"
    );
    for op in resp.operations {
        println!(
            "{:<36}  {:<14}  {:<16}  {:<12}  {:<12}  {}",
            op.id, op.kind, op.phase, op.source_node, op.target_node, op.vm_id
        );
    }
    Ok(())
}

pub async fn get(info: &ConnectionInfo, id: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .get_vm_operation(controller_proto::GetVmOperationRequest { id: id.to_string() })
        .await?
        .into_inner();
    let Some(op) = resp.operation else {
        bail!("operation '{id}' not found");
    };
    println!("Id:               {}", op.id);
    println!("VM:               {}", op.vm_id);
    println!("Kind:             {}", op.kind);
    println!("Phase:            {}", op.phase);
    println!("Source:           {}", op.source_node);
    println!("Target:           {}", op.target_node);
    println!("Cancel requested: {}", op.cancel_requested);
    println!("Send succeeded:   {}", op.send_succeeded);
    println!("Detail:           {}", op.detail_json);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&op.detail_json) {
        if let Some(p) = v.get("progress") {
            let estimate = p.get("estimate").and_then(|x| x.as_bool()).unwrap_or(true);
            let elapsed = p
                .get("elapsedSeconds")
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            let mem = p.get("memoryBytes").and_then(|x| x.as_i64()).unwrap_or(0);
            let msg = p.get("message").and_then(|x| x.as_str()).unwrap_or("");
            println!("Progress (est):   estimate={estimate} elapsed={elapsed}s memoryBytes={mem}");
            if !msg.is_empty() {
                println!("Progress msg:     {msg}");
            }
        }
    }
    Ok(())
}

pub async fn cancel(info: &ConnectionInfo, id: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let resp = client
        .cancel_vm_operation(controller_proto::CancelVmOperationRequest { id: id.to_string() })
        .await?
        .into_inner();
    if !resp.success {
        bail!("{}", resp.message);
    }
    println!("{}", resp.message);
    Ok(())
}
