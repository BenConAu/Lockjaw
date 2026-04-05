fn main() {
    eprintln!("Usage: cargo xtask <command>");
    eprintln!("Commands:");
    eprintln!("  check-stack    Verify stack depth budgets and no recursion");
    std::process::exit(1);
}
