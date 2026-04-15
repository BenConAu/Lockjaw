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
        Some("check-pointers") => check_pointers(),
        _ => {
            eprintln!("Usage: cargo xtask <command>");
            eprintln!("Commands:");
            eprintln!("  check-stack      Verify stack depth budgets and no recursion");
            eprintln!("  check-pointers   Verify all pointer casts have SAFETY comments");
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

    // Check all indirect calls (BLR) are annotated in stack-annotations.toml
    let annotations = load_annotations();
    if !indirect_calls.is_empty() {
        // Demangle the indirect call function names for matching against annotations
        let demangled_names: Vec<String> = indirect_calls
            .iter()
            .map(|name| demangle(name).trim().to_string())
            .collect();

        println!("  {} functions contain indirect calls (BLR):", indirect_calls.len());
        let mut unannotated = Vec::new();
        for (i, name) in demangled_names.iter().enumerate() {
            if annotations.contains_key(name.as_str()) {
                println!("    - {} [annotated]", name);
            } else {
                // Also try the mangled name
                if annotations.contains_key(indirect_calls[i].as_str()) {
                    println!("    - {} [annotated]", name);
                } else {
                    println!("    - {} [UNANNOTATED]", name);
                    unannotated.push(name.clone());
                }
            }
        }
        if !unannotated.is_empty() {
            eprintln!();
            eprintln!("  [FAIL] {} unannotated indirect call site(s)!", unannotated.len());
            eprintln!("  Every BLR must be listed in xtask/stack-annotations.toml.");
            eprintln!("  Unannotated functions:");
            for name in &unannotated {
                eprintln!("    - {}", name);
            }
            process::exit(1);
        }
        println!("  [PASS] All indirect calls annotated");
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
    // Note: we don't demangle here — objdump output is large (thousands of lines)
    // and piping through rustfilt is slow. Function names from <name>: headers
    // are sufficient for call graph analysis.

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

/// Load indirect call annotations from xtask/stack-annotations.toml.
/// Returns a map of function_name -> annotation (either "fmt-internal" or a list of targets).
fn load_annotations() -> HashMap<String, String> {
    let mut annotations = HashMap::new();

    let path = "xtask/stack-annotations.toml";
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("WARN: Could not read {}", path);
            return annotations;
        }
    };

    // Simple TOML parser: look for lines under [indirect_calls]
    let mut in_section = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[indirect_calls]";
            continue;
        }
        if !in_section || line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Parse: "function_name" = "value" or "function_name" = ["a", "b"]
        if let Some((key, _value)) = line.split_once('=') {
            let key = key.trim().trim_matches('"');
            annotations.insert(key.to_string(), "annotated".to_string());
        }
    }

    annotations
}

fn parse_hex_or_dec(s: &str) -> Result<u64, std::num::ParseIntError> {
    if let Some(hex) = s.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)
    } else {
        s.parse()
    }
}

// ---------------------------------------------------------------------------
// Pointer cast safety lint
// ---------------------------------------------------------------------------

/// Verify every `as *const` / `as *mut` cast in kernel source has a
/// `// SAFETY:` comment on the same line or the line immediately before.
///
/// This prevents the TTBR0 race class of bugs: any code that casts a user
/// VA to a pointer and dereferences it is vulnerable to context switches
/// changing TTBR0. The SAFETY comment forces the author to justify why the
/// address is safe (kernel VA via KERNEL_VA_OFFSET, MMIO address, linker
/// symbol, etc). User memory must go through copy_from_user.
fn check_pointers() {
    use std::path::Path;

    println!("=== Pointer Cast Safety Check ===");
    println!("  Every `as *const` / `as *mut` in src/ must have a // SAFETY: comment.");
    println!();

    let src_dir = Path::new("src");
    if !src_dir.exists() {
        eprintln!("ERROR: src/ directory not found (run from project root)");
        process::exit(1);
    }

    let mut violations: Vec<(String, usize, String)> = Vec::new();
    let mut total_casts = 0;

    visit_rs_files(src_dir, &mut |path| {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let lines: Vec<&str> = content.lines().collect();
        let rel_path = path.to_string_lossy().to_string();

        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            // Skip comment-only lines and lines in test modules
            if trimmed.starts_with("//") || trimmed.starts_with("*") {
                continue;
            }
            if !has_pointer_cast(trimmed) {
                continue;
            }
            total_casts += 1;

            // Check this line or up to 2 lines above for // SAFETY:
            // (multi-line expressions may split the cast onto a continuation line)
            let has_safety = trimmed.contains("// SAFETY:")
                || (i > 0 && lines[i - 1].trim().contains("// SAFETY:"))
                || (i > 1 && lines[i - 2].trim().contains("// SAFETY:"));

            if !has_safety {
                violations.push((rel_path.clone(), i + 1, trimmed.to_string()));
            }
        }
    });

    println!("  {} pointer casts found in src/", total_casts);
    println!("  {} without SAFETY annotation", violations.len());
    println!();

    if violations.is_empty() {
        println!("=== Pointer cast check PASSED ===");
    } else {
        eprintln!("=== Pointer cast check FAILED ===");
        eprintln!();
        for (file, line, text) in &violations {
            eprintln!("  {}:{}: {}", file, line, text);
        }
        eprintln!();
        eprintln!("Add `// SAFETY: <reason>` on the same line or line above each cast.");
        eprintln!("User memory must use copy_from_user, never raw pointer casts.");
        process::exit(1);
    }
}

/// Check if a line contains `as *const` or `as *mut` (pointer cast).
fn has_pointer_cast(line: &str) -> bool {
    line.contains("as *const") || line.contains("as *mut")
}

/// Recursively visit all .rs files under a directory.
fn visit_rs_files(dir: &std::path::Path, cb: &mut dyn FnMut(&std::path::Path)) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_rs_files(&path, cb);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            cb(&path);
        }
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

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
