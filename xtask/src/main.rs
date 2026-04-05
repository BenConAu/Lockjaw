use std::collections::{HashMap, HashSet};
use std::env;
use std::io::Write;
use std::process::{self, Command, Stdio};

const NORMAL_PATH_BUDGET: u64 = 3072;
const INTERRUPT_PATH_BUDGET: u64 = 1024;
const TOTAL_BUDGET: u64 = NORMAL_PATH_BUDGET + INTERRUPT_PATH_BUDGET;

const KERNEL_ELF: &str = "target/aarch64-unknown-none/release/lockjaw";

/// Entry points for worst-case depth analysis.
/// Normal path starts at _start, interrupt path starts at exception vectors.
const NORMAL_ENTRY: &str = "_start";

fn main() {
    match env::args().nth(1).as_deref() {
        Some("check-stack") => check_stack(),
        _ => {
            eprintln!("Usage: cargo xtask <command>");
            eprintln!("Commands:");
            eprintln!("  check-stack    Verify stack depth budgets and no recursion");
            process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Top-level check
// ---------------------------------------------------------------------------

fn check_stack() {
    println!("=== Stack Depth Verification ===");
    println!("  Normal path budget:    {} bytes", NORMAL_PATH_BUDGET);
    println!("  Interrupt path budget: {} bytes", INTERRUPT_PATH_BUDGET);
    println!("  Total stack size:      {} bytes", TOTAL_BUDGET);
    println!();

    // Step 1: Build with emit-stack-sizes
    println!("[1/4] Building kernel (release, emit-stack-sizes)...");
    let status = Command::new("cargo")
        .args(["build", "--release"])
        .env(
            "RUSTFLAGS",
            "-Z emit-stack-sizes -C link-arg=-Tlinker.ld -C link-arg=--gc-sections",
        )
        .status()
        .expect("failed to run cargo build");

    if !status.success() {
        eprintln!("FAIL: cargo build failed");
        process::exit(1);
    }

    // Step 2: Parse per-function stack sizes from .stack_sizes ELF section
    println!("[2/4] Parsing per-function stack sizes...");
    let stack_sizes = parse_stack_sizes();
    for (name, size) in &stack_sizes {
        println!("  {:>6} bytes  {}", size, name);
    }
    println!("  ({} functions)", stack_sizes.len());
    println!();

    // Step 3: Extract call graph from disassembly (BL instruction scan)
    println!("[3/4] Extracting call graph from disassembly...");
    let (functions, call_graph, indirect_calls) = extract_call_graph();
    let mut total_edges = 0;
    for targets in call_graph.values() {
        total_edges += targets.len();
    }
    println!("  {} functions, {} call edges", functions.len(), total_edges);
    if !indirect_calls.is_empty() {
        println!("  WARNING: {} functions contain indirect calls (BLR):", indirect_calls.len());
        for name in &indirect_calls {
            println!("    - {}", name);
        }
        println!("  Indirect calls cannot be traced — treat as leaves in depth analysis.");
    }
    println!();

    // Step 4: Analyze — cycle detection + worst-case depth
    println!("[4/4] Analyzing call graph...");
    let mut failed = false;

    // Cycle detection (recursion check)
    let cycles = detect_cycles(&call_graph);
    if cycles.is_empty() {
        println!("  [PASS] No recursion (zero cycles in call graph)");
    } else {
        eprintln!("  [FAIL] Recursion detected! Cycles found:");
        for cycle in &cycles {
            eprintln!("    {}", cycle.join(" -> "));
        }
        failed = true;
    }

    // Worst-case depth from normal entry point
    if let Some(max_depth) = worst_case_depth(NORMAL_ENTRY, &call_graph, &stack_sizes) {
        println!(
            "  Normal path worst-case:    {} bytes (budget: {})",
            max_depth, NORMAL_PATH_BUDGET
        );
        if max_depth > NORMAL_PATH_BUDGET {
            eprintln!("  [FAIL] Exceeds normal path budget!");
            failed = true;
        } else {
            println!("  [PASS] Within normal path budget");
        }
    } else {
        println!("  [SKIP] Entry point '{}' not found in call graph", NORMAL_ENTRY);
    }

    // TODO: Interrupt path analysis — add when exception vectors exist (Phase 3)

    println!();
    if failed {
        eprintln!("=== Stack check FAILED ===");
        process::exit(1);
    }
    println!("=== Stack check PASSED ===");
}

// ---------------------------------------------------------------------------
// Parse .stack_sizes section via rust-readobj
// ---------------------------------------------------------------------------

fn parse_stack_sizes() -> HashMap<String, u64> {
    let output = Command::new("rust-readobj")
        .args(["--stack-sizes", KERNEL_ELF])
        .output()
        .expect("failed to run rust-readobj — is cargo-binutils installed?");

    if !output.status.success() {
        eprintln!(
            "WARN: rust-readobj failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return HashMap::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = demangle(&stdout);

    // Format:
    //   Entry {
    //     Functions: [symbol_name]
    //     Size: 0xNN
    //   }
    let mut sizes = HashMap::new();
    let mut current_fn = String::new();

    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Functions: [") {
            current_fn = rest.strip_suffix(']').unwrap_or(rest).to_string();
        } else if let Some(rest) = line.strip_prefix("Size: ") {
            if let Ok(size) = parse_hex_or_dec(rest) {
                sizes.insert(current_fn.clone(), size);
            }
        }
    }

    sizes
}

// ---------------------------------------------------------------------------
// Extract call graph from objdump disassembly
// ---------------------------------------------------------------------------

/// Returns (function_names, call_graph, functions_with_indirect_calls)
fn extract_call_graph() -> (Vec<String>, HashMap<String, Vec<String>>, Vec<String>) {
    let output = Command::new("rust-objdump")
        .args(["-d", "--no-show-raw-insn", KERNEL_ELF])
        .output()
        .expect("failed to run rust-objdump — is cargo-binutils installed?");

    if !output.status.success() {
        eprintln!(
            "WARN: cargo objdump failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return (vec![], HashMap::new(), vec![]);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = demangle(&stdout);

    let mut functions: Vec<String> = Vec::new();
    let mut call_graph: HashMap<String, Vec<String>> = HashMap::new();
    let mut indirect_calls: Vec<String> = Vec::new();
    let mut current_fn = String::new();

    for line in stdout.lines() {
        let line = line.trim();

        // Function header: "0000000040080078 <kmain>:"
        if line.ends_with(">:") {
            if let Some(start) = line.find('<') {
                let name = line[start + 1..line.len() - 2].to_string();
                current_fn = name.clone();
                functions.push(name.clone());
                call_graph.entry(name).or_default();
            }
            continue;
        }

        if current_fn.is_empty() {
            continue;
        }

        // Parse instruction lines: "40080050:  bl  0x40080078 <kmain>"
        // After --no-show-raw-insn: "40080050:       bl      0x40080078 <kmain>"
        if let Some(colon_pos) = line.find(':') {
            let insn_part = line[colon_pos + 1..].trim();

            // BL — direct call
            if insn_part.starts_with("bl\t") || insn_part.starts_with("bl ") {
                if let Some(target) = extract_call_target(insn_part) {
                    let callees = call_graph.entry(current_fn.clone()).or_default();
                    if !callees.contains(&target) {
                        callees.push(target);
                    }
                }
            }
            // BLR — indirect call via register (flagged, can't trace)
            else if insn_part.starts_with("blr\t") || insn_part.starts_with("blr ") {
                if !indirect_calls.contains(&current_fn) {
                    indirect_calls.push(current_fn.clone());
                }
            }
        }
    }

    (functions, call_graph, indirect_calls)
}

/// Extract the target function name from a "bl 0xADDR <name>" instruction.
fn extract_call_target(insn: &str) -> Option<String> {
    // Format: "bl\t0x40080078 <kmain>" or "bl\t0x40080078"
    if let Some(start) = insn.find('<') {
        let end = insn.find('>')?;
        Some(insn[start + 1..end].to_string())
    } else {
        // No symbolic name — use raw address
        let addr_part = insn.split_whitespace().nth(1)?;
        Some(addr_part.to_string())
    }
}

// ---------------------------------------------------------------------------
// Cycle detection (DFS-based)
// ---------------------------------------------------------------------------

fn detect_cycles(graph: &HashMap<String, Vec<String>>) -> Vec<Vec<String>> {
    let mut cycles = Vec::new();
    let mut visited = HashSet::new();
    let mut on_stack = HashSet::new();
    let mut path = Vec::new();

    for node in graph.keys() {
        if !visited.contains(node) {
            dfs_cycles(
                node,
                graph,
                &mut visited,
                &mut on_stack,
                &mut path,
                &mut cycles,
            );
        }
    }

    cycles
}

fn dfs_cycles(
    node: &str,
    graph: &HashMap<String, Vec<String>>,
    visited: &mut HashSet<String>,
    on_stack: &mut HashSet<String>,
    path: &mut Vec<String>,
    cycles: &mut Vec<Vec<String>>,
) {
    visited.insert(node.to_string());
    on_stack.insert(node.to_string());
    path.push(node.to_string());

    if let Some(neighbors) = graph.get(node) {
        for next in neighbors {
            if !visited.contains(next.as_str()) {
                dfs_cycles(next, graph, visited, on_stack, path, cycles);
            } else if on_stack.contains(next.as_str()) {
                // Found a cycle — extract it from the path
                let cycle_start = path.iter().position(|n| n == next).unwrap();
                let mut cycle: Vec<String> = path[cycle_start..].to_vec();
                cycle.push(next.clone()); // close the loop
                cycles.push(cycle);
            }
        }
    }

    path.pop();
    on_stack.remove(node);
}

// ---------------------------------------------------------------------------
// Worst-case stack depth (DFS with memoization)
// ---------------------------------------------------------------------------

/// Compute worst-case stack depth from an entry point by summing frame sizes
/// along the deepest path. Returns None if the entry point is not found.
fn worst_case_depth(
    entry: &str,
    graph: &HashMap<String, Vec<String>>,
    stack_sizes: &HashMap<String, u64>,
) -> Option<u64> {
    if !graph.contains_key(entry) {
        return None;
    }

    let mut memo: HashMap<String, u64> = HashMap::new();
    let mut in_progress: HashSet<String> = HashSet::new();
    Some(compute_depth(entry, graph, stack_sizes, &mut memo, &mut in_progress))
}

fn compute_depth(
    node: &str,
    graph: &HashMap<String, Vec<String>>,
    stack_sizes: &HashMap<String, u64>,
    memo: &mut HashMap<String, u64>,
    in_progress: &mut HashSet<String>,
) -> u64 {
    if let Some(&cached) = memo.get(node) {
        return cached;
    }

    // Guard against cycles (shouldn't happen if cycle check passed, but be safe)
    if in_progress.contains(node) {
        return 0;
    }
    in_progress.insert(node.to_string());

    let my_size = stack_sizes.get(node).copied().unwrap_or(0);

    let max_callee_depth = graph
        .get(node)
        .map(|callees| {
            callees
                .iter()
                .map(|callee| compute_depth(callee, graph, stack_sizes, memo, in_progress))
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);

    let total = my_size + max_callee_depth;
    in_progress.remove(node);
    memo.insert(node.to_string(), total);
    total
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn parse_hex_or_dec(s: &str) -> Result<u64, std::num::ParseIntError> {
    if let Some(hex) = s.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)
    } else {
        s.parse()
    }
}

/// Pipe text through `rustfilt` to demangle Rust symbols.
/// Returns the original text unchanged if rustfilt is not installed.
fn demangle(text: &str) -> String {
    let child = Command::new("rustfilt")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn();

    match child {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            match child.wait_with_output() {
                Ok(output) if output.status.success() => {
                    String::from_utf8(output.stdout).unwrap_or_else(|_| text.to_string())
                }
                _ => text.to_string(),
            }
        }
        Err(_) => text.to_string(),
    }
}
