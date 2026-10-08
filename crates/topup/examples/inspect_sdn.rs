//! Offline verification using the production parser and the selected Cargo profile.
fn main() -> anyhow::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: inspect_sdn SDN.XML"))?;
    let summary = topup::sanctions::inspect_publication(&std::fs::read(path)?)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}
