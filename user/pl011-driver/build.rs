include!("../../build-support/hash.rs");

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    write_source_hash(&root);
    println!("cargo:rerun-if-changed=../../target/source-hash.txt");
}
