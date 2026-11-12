//! Repository maintenance entry point; not an installed Clank command.
mod support;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("notices") if args.len() >= 4 => {
            let about = serde_json::from_slice(&std::fs::read(&args[1])?)?;
            let metadata: serde_json::Value = serde_json::from_slice(&std::fs::read(&args[2])?)?;
            let members = metadata["workspace_members"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Cargo metadata has no workspace_members"))?
                .iter()
                .map(|id| {
                    id.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow::anyhow!("invalid workspace package id"))
                })
                .collect::<anyhow::Result<_>>()?;
            print!(
                "{}",
                support::notices::render(&about, &members, &args[3], &args[4..])?
            );
        }
        Some("release-check") if matches!(args.len(), 3 | 4) => {
            support::release::run(
                std::path::Path::new(&args[1]),
                &args[2],
                args.get(3).map(std::path::Path::new),
            )?;
        }
        _ => anyhow::bail!(
            "usage: maintenance release-check <workspace> <tag> [binary]\n       maintenance notices <about.json> <metadata.json> <about-version> [targets...]"
        ),
    }
    Ok(())
}
