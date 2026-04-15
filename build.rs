include!("build-support/hash.rs");

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    write_source_hash(root);

    // Track the init binary so cargo rebuilds the kernel when it changes
    println!("cargo:rerun-if-changed=user/init/target/aarch64-unknown-none/release/lockjaw-init");

    // Track the hash file written by `make build-hash` (P1 rebuild trigger)
    println!("cargo:rerun-if-changed=target/source-hash.txt");
}
