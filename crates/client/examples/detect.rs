//! Dev helper: print every instance Bonegrader can auto-detect on this machine.
//!
//! Run with: `cargo run --example detect -p bonegrader-client`

fn main() {
    let found = bonegrader_client::detect::discover_all();
    if found.is_empty() {
        println!("No instances auto-detected.");
        return;
    }
    println!("Detected {} instance(s):\n", found.len());
    for i in &found {
        println!(
            "  [{:?}] {}\n      path:   {}\n      loader: {} {}  (MC {})",
            i.launcher,
            i.name,
            i.path.display(),
            i.loader_type.as_deref().unwrap_or("?"),
            i.loader_version.as_deref().unwrap_or("?"),
            i.mc_version.as_deref().unwrap_or("?"),
        );
    }
}
