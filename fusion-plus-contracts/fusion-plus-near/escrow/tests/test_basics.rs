#[tokio::test]
async fn contract_deploys() -> Result<(), Box<dyn std::error::Error>> {
    // Path to wasm built by `cargo near build`
    let wasm_path = format!("{}/../target/near/escrow.wasm", env!("CARGO_MANIFEST_DIR"));
    let wasm = std::fs::read(wasm_path)?;

    let sandbox = near_workspaces::sandbox().await?;
    // Deploy should succeed
    let _contract = sandbox.dev_deploy(&wasm).await?;

    Ok(())
}
