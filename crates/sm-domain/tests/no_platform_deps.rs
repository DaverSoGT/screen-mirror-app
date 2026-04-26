//! Hexagonal invariant test: `sm-domain` must have zero platform-specific or runtime crates
//! in its dependency closure.
//!
//! This test enforces R12.1 and AC #17 automatically on every CI run. It fails immediately
//! when any contributor accidentally adds a banned crate to `sm-domain/Cargo.toml`.

/// Crates that are strictly forbidden from appearing in `sm-domain`'s resolved dependency graph.
const BANNED: &[&str] = &[
    "tokio",
    "windows",
    "windows-capture",
    "windows-sys",
    "windows-targets",
    "tauri",
    "wasm-bindgen",
    "openh264", // encoder backend must stay in sm-infra (R10.2)
];

#[test]
fn sm_domain_dep_graph_has_no_platform_or_runtime_crates() {
    let metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .exec()
        .expect("cargo metadata should succeed");

    let sm_domain_pkg = metadata
        .packages
        .iter()
        .find(|p| p.name == "sm-domain")
        .expect("sm-domain package must be present in metadata");

    // Resolved dependency node for sm-domain.
    let resolve = metadata
        .resolve
        .as_ref()
        .expect("resolve section must be present (no --no-deps)");

    let sm_domain_node = resolve
        .nodes
        .iter()
        .find(|n| n.id == sm_domain_pkg.id)
        .expect("sm-domain node must be present in resolve graph");

    // Collect direct+transitive dependency names reachable from sm-domain.
    let mut dep_names: Vec<&str> = sm_domain_node
        .deps
        .iter()
        .map(|d| d.name.as_str())
        .collect();

    // Sort for deterministic error messages.
    dep_names.sort_unstable();

    for &banned in BANNED {
        assert!(
            !dep_names.contains(&banned),
            "BANNED CRATE detected in sm-domain dependency graph: '{}'\n\
             Found deps: {:?}\n\
             sm-domain MUST remain platform-agnostic (R12.1).",
            banned,
            dep_names,
        );
    }
}
