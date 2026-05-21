use std::collections::{HashMap, HashSet};
use std::env;
use std::process::{self, Command, Stdio};

mod gen_regs;
mod gen_wires;

/// Per-function stack frame cap in bytes. Any single function exceeding
/// this fails immediately — catches large locals before they interact
/// with call depth. Set to 1600 to accommodate create_process's frame
/// (~1552 in debug) — the AddressSpaceBuilder struct lives on its stack.
/// See docs/stack-budget.md.
const PER_FUNCTION_CAP: u64 = 1600;

/// Total kernel stack budget: normal path + worst-case nested exception.
const TOTAL_BUDGET: u64 = 8192;

const KERNEL_ELF_DEBUG: &str = "target/aarch64-unknown-none/debug/lockjaw";
const KERNEL_ELF_RELEASE: &str = "target/aarch64-unknown-none/release/lockjaw";

/// Entry points for worst-case depth analysis.
const NORMAL_ENTRY: &str = "_start";
const SECONDARY_ENTRY: &str = "_secondary_start";
const SYNC_EXCEPTION_ENTRY: &str = "__vec_sync_lower";
const IRQ_EXCEPTION_ENTRY: &str = "__vec_irq";

const ANNOTATIONS_PATH: &str = "xtask/stack-annotations.toml";

fn main() {
    let cmd = env::args().nth(1);
    let rest: Vec<String> = env::args().skip(2).collect();
    match cmd.as_deref() {
        Some("check-stack") => check_stack(),
        Some("check-pointers") => check_pointers(),
        Some("check-vtables") => check_vtables(),
        Some("check-init-size") => check_init_size(),
        Some("check-linker-symbols") => check_linker_symbols(),
        Some("gen-regs") => {
            let check = rest.iter().any(|a| a == "--check");
            gen_regs::run(check);
        }
        Some("gen-wires") => {
            let check = rest.iter().any(|a| a == "--check");
            gen_wires::run(check);
        }
        _ => {
            eprintln!("Usage: cargo xtask <command>");
            eprintln!("Commands:");
            eprintln!("  check-stack            Verify stack depth budgets and no recursion");
            eprintln!("  check-pointers         Verify all pointer casts have SAFETY comments");
            eprintln!("  check-vtables          Scan data sections for absolute code pointers");
            eprintln!("  check-init-size        Verify init ELF fits in kernel mapping buffer");
            eprintln!("  check-linker-symbols   Enforce docs/linker-symbol-audit.md allowlist");
            eprintln!("  gen-regs [--check]     Generate lockjaw-regs from user/regspecs/*.toml");
            eprintln!("  gen-wires [--check]    Generate lockjaw-types::wire from user/wirespecs/*.toml");
            process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Top-level check
// ---------------------------------------------------------------------------

fn check_stack() {
    // Verify rustfilt is available — needed for consistent symbol demangling.
    ensure_tool("rustfilt", "cargo install rustfilt");

    // Debug first — it's what `make test` runs and has larger frames.
    check_stack_for_profile("debug", KERNEL_ELF_DEBUG);
    println!();
    check_stack_for_profile("release", KERNEL_ELF_RELEASE);
}

fn check_stack_for_profile(profile: &str, elf_path: &str) {
    println!("=== Stack Depth Verification ({}) ===", profile);
    println!("  Per-function cap: {} bytes", PER_FUNCTION_CAP);
    println!("  Total budget (normal + exception): {} bytes", TOTAL_BUDGET);
    println!();

    // Build kernel. emit-stack-sizes is in .cargo/config.toml, so no
    // RUSTFLAGS override needed — we check the same binary tests run.
    println!("Building kernel ({})...", profile);
    let mut build_args = vec!["build"];
    if profile == "release" {
        build_args.push("--release");
    }
    let status = Command::new("cargo")
        .args(&build_args)
        .status()
        .expect("failed to run cargo build");
    if !status.success() {
        eprintln!("FAIL: cargo build ({}) failed", profile);
        process::exit(1);
    }
    println!();

    let annotations = load_annotations();
    let mut failed = false;

    // [1/5] Stack sizes from ELF .stack_sizes section
    println!("[1/5] Reading stack sizes from {} ELF...", profile);
    let mut stack_sizes = parse_stack_sizes(elf_path);
    println!("  {} functions with compiler stack size data", stack_sizes.len());

    // [2/5] Call graph from disassembly
    println!();
    println!("[2/5] Extracting call graph...");
    let (functions, mut call_graph, indirect_fns, bl_count, tail_count) =
        extract_call_graph(elf_path);
    println!(
        "  {} functions, {} call edges (bl), {} tail edges (b)",
        functions.len(),
        bl_count,
        tail_count
    );

    // Validate known_assembly symbols exist in disassembly.
    // For generic functions (e.g. assert_failed), accept a match
    // if any monomorphized instance (assert_failed::<u64, u64>) exists.
    // For wildcard entries (key ends with '*'), accept any
    // function whose name starts with the prefix (used for linker-
    // generated stubs with build-dependent address suffixes — see
    // `__CortexA53843419_*`).
    // External symbols (core::, mem*) may be absent in release builds
    // (optimized away) — only warn, don't fail.
    for sym in annotations.known_assembly.keys() {
        if functions.contains(sym.as_str()) {
            continue;
        }
        if let Some(prefix) = sym.strip_suffix('*') {
            if functions.iter().any(|f| f.starts_with(prefix)) {
                continue;
            }
            // Wildcard entry with no matching function — soft skip
            // (the stub may simply not have been generated this build).
            continue;
        }
        let has_generic_instance = functions
            .iter()
            .any(|f| f.starts_with(sym.as_str()) && f[sym.len()..].starts_with("::<"));
        if has_generic_instance {
            continue;
        }
        // External symbols may be absent in optimized builds
        let is_external = sym.starts_with("core::")
            || sym.starts_with("OUTLINED_FUNCTION")
            || sym == "memcpy"
            || sym == "memset"
            || sym == "memcmp";
        if is_external {
            // Absent external — silently skip, not needed for analysis
            continue;
        }
        eprintln!(
            "FAIL: [known_assembly] symbol '{}' not found in disassembly",
            sym
        );
        eprintln!("Symbol may have been renamed or deleted.");
        process::exit(1);
    }

    // Merge known_assembly sizes into stack_sizes
    for (sym, size) in &annotations.known_assembly {
        stack_sizes.insert(sym.clone(), *size);
    }
    if !annotations.known_assembly.is_empty() {
        println!(
            "  {} assembly functions from [known_assembly]",
            annotations.known_assembly.len()
        );
    }

    // Check all indirect calls (BLR) are annotated
    if !indirect_fns.is_empty() {
        println!(
            "  {} functions contain indirect calls (BLR):",
            indirect_fns.len()
        );
        let mut unannotated = Vec::new();
        for name in &indirect_fns {
            if annotations.indirect_calls.contains_key(name.as_str()) {
                println!("    - {} [annotated]", name);
            } else {
                println!("    - {} [UNANNOTATED]", name);
                unannotated.push(name.clone());
            }
        }
        if !unannotated.is_empty() {
            eprintln!();
            eprintln!(
                "  FAIL: {} unannotated indirect call site(s)",
                unannotated.len()
            );
            eprintln!("  Every BLR must be listed in {}", ANNOTATIONS_PATH);
            for name in &unannotated {
                eprintln!("    - {}", name);
            }
            process::exit(1);
        }
        println!("  All indirect calls annotated");
    }

    // Resolve indirect call annotation targets into call graph edges.
    // Skip annotations for functions that were inlined away in this
    // profile — their BLR sites are absorbed into the inlining caller.
    let mut resolved_count = 0;
    for (fn_name, annotation) in &annotations.indirect_calls {
        if !call_graph.contains_key(fn_name) {
            continue;
        }
        if let IndirectAnnotation::Targets(targets) = annotation {
            for target in targets {
                if let Some(resolved) = resolve_target_name(target, &functions) {
                    let callees = call_graph.entry(fn_name.clone()).or_default();
                    if !callees.contains(&resolved) {
                        callees.push(resolved);
                        resolved_count += 1;
                    }
                } else {
                    eprintln!(
                        "FAIL: indirect call target '{}' (from '{}') not found in disassembly",
                        target, fn_name
                    );
                    process::exit(1);
                }
            }
        }
    }
    if resolved_count > 0 {
        println!(
            "  Resolved {} indirect-call targets into graph",
            resolved_count
        );
    }

    // Check all reachable functions have stack size data.
    let entry_points = [NORMAL_ENTRY, SECONDARY_ENTRY, SYNC_EXCEPTION_ENTRY, IRQ_EXCEPTION_ENTRY];
    let reachable = collect_reachable(&entry_points, &call_graph);
    let mut missing: Vec<String> = reachable
        .iter()
        .filter(|name| lookup_stack_size(name, &stack_sizes).is_none())
        .cloned()
        .collect();
    missing.sort();
    if !missing.is_empty() {
        eprintln!();
        eprintln!(
            "FAIL: {} reachable function(s) have no stack size data:",
            missing.len()
        );
        for name in &missing {
            eprintln!("  - {}", name);
        }
        eprintln!();
        eprintln!("This can happen if the function is assembly-only or was stripped.");
        eprintln!(
            "Add it to [known_assembly] in {} with a",
            ANNOTATIONS_PATH
        );
        eprintln!("manually measured size, or fix the build to emit stack sizes.");
        process::exit(1);
    }

    // [3/5] Per-function frame cap
    println!();
    println!(
        "[3/5] Checking per-function frame sizes (cap: {})...",
        PER_FUNCTION_CAP
    );
    let (largest_fn, largest_size) = stack_sizes
        .iter()
        .max_by_key(|(_, &size)| size)
        .map(|(name, &size)| (name.clone(), size))
        .unwrap_or_default();
    println!("  Largest: {} ({} bytes)", largest_fn, largest_size);

    let mut over_cap: Vec<(String, u64)> = stack_sizes
        .iter()
        .filter(|(_, &size)| size > PER_FUNCTION_CAP)
        .map(|(name, &size)| (name.clone(), size))
        .collect();
    over_cap.sort_by(|a, b| b.1.cmp(&a.1));

    if over_cap.is_empty() {
        println!("  [PASS] All functions within cap");
    } else {
        for (name, size) in &over_cap {
            eprintln!(
                "  FAIL: {} uses {} bytes (per-function cap: {})",
                name, size, PER_FUNCTION_CAP
            );
        }
        failed = true;
    }

    // [4/5] Path analysis — cycle detection + worst-case depth
    println!();
    println!("[4/5] Analyzing paths...");

    // Cycle detection (recursion check).
    let all_cycles = detect_cycles(&call_graph);
    let real_cycles: Vec<_> = all_cycles
        .into_iter()
        .filter(|cycle| {
            !cycle
                .iter()
                .any(|name| annotations.allowed_cycles.contains(name.as_str()))
        })
        .collect();

    if real_cycles.is_empty() {
        println!("  No unguarded cycles");
    } else {
        eprintln!("  FAIL: Recursion detected! Unguarded cycles:");
        for cycle in &real_cycles {
            eprintln!("    {}", cycle.join(" -> "));
        }
        failed = true;
    }

    let normal_depth = require_depth(NORMAL_ENTRY, &call_graph, &stack_sizes);
    println!("  Normal ({}): worst-case {} bytes", NORMAL_ENTRY, normal_depth);

    let secondary_depth = require_depth(SECONDARY_ENTRY, &call_graph, &stack_sizes);
    println!("  Secondary ({}): worst-case {} bytes", SECONDARY_ENTRY, secondary_depth);

    let sync_depth = require_depth(SYNC_EXCEPTION_ENTRY, &call_graph, &stack_sizes);
    println!("  Sync exception ({}): worst-case {} bytes", SYNC_EXCEPTION_ENTRY, sync_depth);

    let irq_depth = require_depth(IRQ_EXCEPTION_ENTRY, &call_graph, &stack_sizes);
    println!("  IRQ exception ({}): worst-case {} bytes", IRQ_EXCEPTION_ENTRY, irq_depth);

    // [5/5] Combined check — worst normal-mode path + worst exception ≤ total budget
    // Normal-mode paths: primary boot (_start) and secondary boot (_secondary_start)
    // both run on per-CPU kernel stacks.
    println!();
    println!("[5/5] Combined check...");
    let worst_normal = normal_depth.max(secondary_depth);
    let exception_depth = sync_depth.max(irq_depth);
    let combined = worst_normal + exception_depth;
    println!(
        "  max(normal {}, secondary {}) + max(sync {}, irq {}) = {} bytes (budget: {})",
        normal_depth, secondary_depth, sync_depth, irq_depth, combined, TOTAL_BUDGET
    );

    if combined > TOTAL_BUDGET {
        eprintln!("  FAIL: Exceeds total budget!");
        failed = true;
    } else {
        println!("  [PASS]");
    }

    println!();
    if failed {
        eprintln!("=== Stack check FAILED ({}) ===", profile);
        process::exit(1);
    }
    println!("=== Stack check PASSED ({}) ===", profile);
}

// ---------------------------------------------------------------------------
// Parse .stack_sizes section via rust-readobj
// ---------------------------------------------------------------------------

fn parse_stack_sizes(elf_path: &str) -> HashMap<String, u64> {
    let output = Command::new("rust-readobj")
        .args(["--stack-sizes", elf_path])
        .output()
        .expect("failed to run rust-readobj — is cargo-binutils installed?");

    if !output.status.success() {
        eprintln!("FAIL: rust-readobj --stack-sizes failed:");
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        process::exit(1);
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

/// Returns (function_set, call_graph, indirect_call_fns, bl_count, tail_count)
fn extract_call_graph(
    elf_path: &str,
) -> (
    HashSet<String>,
    HashMap<String, Vec<String>>,
    Vec<String>,
    usize,
    usize,
) {
    let output = Command::new("rust-objdump")
        .args(["-d", "--no-show-raw-insn", "--demangle", elf_path])
        .output()
        .expect("failed to run rust-objdump — is cargo-binutils installed?");

    if !output.status.success() {
        eprintln!("FAIL: rust-objdump failed:");
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        process::exit(1);
    }

    // --demangle handles symbol demangling inline (no rustfilt pipe
    // needed for the large disassembly output).
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut functions = HashSet::new();
    let mut call_graph: HashMap<String, Vec<String>> = HashMap::new();
    let mut indirect_fns: Vec<String> = Vec::new();
    let mut current_fn = String::new();
    let mut bl_count: usize = 0;
    let mut tail_count: usize = 0;

    for line in stdout.lines() {
        let line = line.trim();

        // Function header: "0000000040080078 <kmain>:"
        if line.ends_with(">:") {
            if let Some(start) = line.find('<') {
                // Use rfind for '>' to handle demangled generics like Result<T, E>
                let name = line[start + 1..line.len() - 2].to_string();
                current_fn = name.clone();
                functions.insert(name.clone());
                call_graph.entry(name).or_default();
            }
            continue;
        }

        if current_fn.is_empty() {
            continue;
        }

        // Parse instruction: "40080050:       bl      0x40080078 <kmain>"
        if let Some(colon_pos) = line.find(':') {
            let insn_part = line[colon_pos + 1..].trim();
            let mnemonic = insn_part.split_whitespace().next().unwrap_or("");

            match mnemonic {
                // BL — direct call
                "bl" => {
                    if let Some(target) = extract_call_target(insn_part) {
                        let callees = call_graph.entry(current_fn.clone()).or_default();
                        if !callees.contains(&target) {
                            callees.push(target);
                            bl_count += 1;
                        }
                    }
                }
                // B — unconditional branch, potential tail call.
                // Only count inter-function branches: target is a different
                // symbol with no +offset (which indicates an internal branch
                // within the same or another function body).
                "b" => {
                    if let Some(target) = extract_call_target(insn_part) {
                        if !target.contains('+') && target != current_fn {
                            let callees = call_graph.entry(current_fn.clone()).or_default();
                            if !callees.contains(&target) {
                                callees.push(target);
                                tail_count += 1;
                            }
                        }
                    }
                }
                // BLR — indirect call via register (can't trace statically)
                "blr" => {
                    if !indirect_fns.contains(&current_fn) {
                        indirect_fns.push(current_fn.clone());
                    }
                }
                _ => {}
            }
        }
    }

    (functions, call_graph, indirect_fns, bl_count, tail_count)
}

/// Extract the target function name from a call/branch instruction.
/// Handles demangled names with nested angle brackets (e.g. Result<T, E>).
fn extract_call_target(insn: &str) -> Option<String> {
    let start = insn.find('<')?;
    let end = insn.rfind('>')?;
    if end > start {
        Some(insn[start + 1..end].to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Annotation loading
// ---------------------------------------------------------------------------

enum IndirectAnnotation {
    /// Unresolvable formatter dispatch — no edges added to call graph.
    FmtInternal,
    /// Known call targets — edges added to call graph.
    Targets(Vec<String>),
}

struct Annotations {
    indirect_calls: HashMap<String, IndirectAnnotation>,
    allowed_cycles: HashSet<String>,
    known_assembly: HashMap<String, u64>,
    allowed_vtables: HashMap<String, String>,
}

fn load_annotations() -> Annotations {
    let content = match std::fs::read_to_string(ANNOTATIONS_PATH) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("FAIL: could not read {}: {}", ANNOTATIONS_PATH, e);
            process::exit(1);
        }
    };

    let mut indirect_calls = HashMap::new();
    let mut allowed_cycles = HashSet::new();
    let mut known_assembly = HashMap::new();
    let mut allowed_vtables = HashMap::new();

    enum Section {
        None,
        IndirectCalls,
        AllowedCycles,
        KnownAssembly,
        AllowedVtables,
    }
    let mut section = Section::None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line {
            "[indirect_calls]" => {
                section = Section::IndirectCalls;
                continue;
            }
            "[allowed_cycles]" => {
                section = Section::AllowedCycles;
                continue;
            }
            "[known_assembly]" => {
                section = Section::KnownAssembly;
                continue;
            }
            "[allowed_vtables]" => {
                section = Section::AllowedVtables;
                continue;
            }
            _ if line.starts_with('[') => {
                section = Section::None;
                continue;
            }
            _ => {}
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches('"').to_string();
        let value = value.trim();

        match section {
            Section::IndirectCalls => {
                if value == "\"fmt-internal\"" {
                    indirect_calls.insert(key, IndirectAnnotation::FmtInternal);
                } else if value.starts_with('[') {
                    // Parse array: ["a", "b", "c"]
                    let inner = value.trim_start_matches('[').trim_end_matches(']');
                    let targets: Vec<String> = inner
                        .split(',')
                        .map(|s| s.trim().trim_matches('"').to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    indirect_calls.insert(key, IndirectAnnotation::Targets(targets));
                }
            }
            Section::AllowedCycles => {
                allowed_cycles.insert(key);
            }
            Section::KnownAssembly => {
                if let Ok(size) = parse_hex_or_dec(value) {
                    known_assembly.insert(key, size);
                }
            }
            Section::AllowedVtables => {
                let reason = value.trim_matches('"').to_string();
                allowed_vtables.insert(key, reason);
            }
            Section::None => {}
        }
    }

    Annotations {
        indirect_calls,
        allowed_cycles,
        known_assembly,
        allowed_vtables,
    }
}

/// Resolve an annotation target name to a function in the disassembly.
/// Tries exact match first, then suffix match (::name).
fn resolve_target_name(target: &str, functions: &HashSet<String>) -> Option<String> {
    // Exact match
    if functions.contains(target) {
        return Some(target.to_string());
    }
    // Suffix match: look for functions ending in ::target
    let suffix = format!("::{}", target);
    let matches: Vec<&String> = functions.iter().filter(|f| f.ends_with(&suffix)).collect();
    match matches.len() {
        1 => Some(matches[0].clone()),
        0 => None,
        _ => {
            eprintln!(
                "WARN: annotation target '{}' matches multiple functions:",
                target
            );
            for m in &matches {
                eprintln!("  - {}", m);
            }
            // Ambiguous — treat as unresolved
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Reachability
// ---------------------------------------------------------------------------

/// Collect all functions reachable from the given entry points via BFS.
fn collect_reachable(
    entries: &[&str],
    graph: &HashMap<String, Vec<String>>,
) -> HashSet<String> {
    let mut reachable = HashSet::new();
    let mut worklist: Vec<String> = entries
        .iter()
        .filter(|&&e| graph.contains_key(e))
        .map(|&e| e.to_string())
        .collect();

    while let Some(node) = worklist.pop() {
        if !reachable.insert(node.clone()) {
            continue;
        }
        if let Some(callees) = graph.get(&node) {
            for callee in callees {
                if !reachable.contains(callee) {
                    worklist.push(callee.clone());
                }
            }
        }
    }

    reachable
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

/// Compute worst-case depth from a required entry point, exiting on failure.
fn require_depth(
    entry: &str,
    graph: &HashMap<String, Vec<String>>,
    stack_sizes: &HashMap<String, u64>,
) -> u64 {
    worst_case_depth(entry, graph, stack_sizes).unwrap_or_else(|| {
        eprintln!("FAIL: entry point '{}' not found in call graph", entry);
        process::exit(1);
    })
}

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
    Some(compute_depth(
        entry,
        graph,
        stack_sizes,
        &mut memo,
        &mut in_progress,
    ))
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

    let my_size = lookup_stack_size(node, stack_sizes).unwrap_or(0);

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

/// Strip LLVM internal suffix like " (.llvm.13824057243090990755)" from
/// function names. These are LLVM-duplicated specializations that share
/// the same stack frame as the original.
fn strip_llvm_suffix(name: &str) -> &str {
    if let Some(idx) = name.find(" (.llvm.") {
        &name[..idx]
    } else {
        name
    }
}

/// Look up a function's stack size, trying in order:
/// 1. Exact match
/// 2. With .llvm.NNNN suffix stripped
/// 3. Generic prefix match (name::<T> matches base name)
/// 4. Wildcard match (key ending with `*` matches any name starting
///    with the prefix — used for linker-generated stubs whose
///    address suffix shifts between builds)
fn lookup_stack_size(name: &str, sizes: &HashMap<String, u64>) -> Option<u64> {
    if let Some(&s) = sizes.get(name) {
        return Some(s);
    }
    let stripped = strip_llvm_suffix(name);
    if stripped != name {
        if let Some(&s) = sizes.get(stripped) {
            return Some(s);
        }
    }
    // Generic monomorphization: "foo::bar::<u64, u64>" matches "foo::bar"
    if let Some((_, &s)) = sizes
        .iter()
        .find(|(key, _)| {
            name.starts_with(key.as_str())
                && name.len() > key.len()
                && name[key.len()..].starts_with("::<")
        })
    {
        return Some(s);
    }
    // Wildcard: "__CortexA53843419_*" matches any
    // "__CortexA53843419_FFFF008000010004" etc.
    sizes
        .iter()
        .find_map(|(key, &s)| {
            key.strip_suffix('*')
                .filter(|prefix| name.starts_with(prefix))
                .map(|_| s)
        })
}

fn parse_hex_or_dec(s: &str) -> Result<u64, std::num::ParseIntError> {
    if let Some(hex) = s.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)
    } else {
        s.parse()
    }
}

/// Pipe text through `rustfilt` to demangle Rust symbols.
/// Returns the original text unchanged if rustfilt is not installed.
///
/// Uses a temp file for input to avoid pipe buffer deadlock.
fn demangle(text: &str) -> String {
    let tmp = format!("/tmp/lockjaw-rustfilt-{}.tmp", std::process::id());
    if std::fs::write(&tmp, text).is_err() {
        return text.to_string();
    }
    let file = match std::fs::File::open(&tmp) {
        Ok(f) => f,
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            return text.to_string();
        }
    };
    let result = Command::new("rustfilt").stdin(file).output();
    let _ = std::fs::remove_file(&tmp);
    match result {
        Ok(output) if output.status.success() => {
            String::from_utf8(output.stdout).unwrap_or_else(|_| text.to_string())
        }
        _ => text.to_string(),
    }
}

/// Check that a required tool is on PATH.
fn ensure_tool(name: &str, install_hint: &str) {
    let result = Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match result {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("FAIL: '{}' not found. Install with: {}", name, install_hint);
            process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Data-to-code pointer check (vtable / jump table / fn pointer detection)
// ---------------------------------------------------------------------------

/// Scan .rodata and .data for 8-byte aligned values that fall within
/// the .text virtual address range. These are absolute code pointers
/// baked in at link time — the exact mechanism behind vtable hazards,
/// jump tables, and function pointer arrays that break when the kernel
/// is loaded at a different physical address than the link address.
fn check_vtables() {
    println!("=== Data-to-Code Pointer Check ===");
    println!("  Scans .rodata/.data for absolute code pointers.");
    println!();

    let elf_path = KERNEL_ELF_DEBUG;
    let elf = match std::fs::read(elf_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("FAIL: cannot read {}: {}", elf_path, e);
            eprintln!("Run `cargo build` first.");
            process::exit(1);
        }
    };

    if elf.len() < 64 || &elf[..4] != b"\x7fELF" {
        eprintln!("FAIL: {} is not a valid ELF file", elf_path);
        process::exit(1);
    }

    let sections = parse_elf_sections(&elf);

    let text = sections.iter().find(|s| s.name == ".text");
    let text = match text {
        Some(s) => s,
        None => {
            eprintln!("FAIL: .text section not found in {}", elf_path);
            process::exit(1);
        }
    };
    let text_start = text.vaddr;
    let text_end = text.vaddr + text.size as u64;

    println!("  .text range: {:#x}..{:#x} ({} bytes)", text_start, text_end, text.size);

    // Scan .rodata, .data, and .data.rel.ro for code pointers
    let scan_names = [".rodata", ".data", ".data.rel.ro"];
    let data_sections: Vec<&ElfSection> = sections
        .iter()
        .filter(|s| scan_names.contains(&s.name.as_str()))
        .collect();

    let mut hits: Vec<(String, u64, u64)> = Vec::new();
    for sec in &data_sections {
        let end = sec.file_offset + sec.size;
        if end > elf.len() {
            eprintln!("WARN: {} extends past EOF, skipping", sec.name);
            continue;
        }
        let bytes = &elf[sec.file_offset..end];
        let mut off = 0;
        while off + 8 <= sec.size {
            let val = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
            if val >= text_start && val < text_end {
                hits.push((sec.name.clone(), sec.vaddr + off as u64, val));
            }
            off += 8;
        }
    }

    if hits.is_empty() {
        println!("  No code pointers in data sections.");
        println!();
        println!("=== Data-to-code check PASSED ===");
        return;
    }

    // Resolve target addresses to symbol names via rust-nm
    let nm_map = build_symbol_map(elf_path);

    let annotations = load_annotations();

    // Filter against allowlist. Two-pass: first check direct name match,
    // then for intra-function pointers check if nearby code calls an
    // allowed function (covers kprintln! infrastructure that shifts with
    // unrelated code changes).
    let mut violations = Vec::new();
    for (sec, offset, target) in &hits {
        let name = resolve_code_pointer(*target, &nm_map);
        let direct_match = annotations.allowed_vtables.iter().any(|(pattern, _)| {
            name.contains(pattern.as_str())
        });
        if direct_match {
            continue;
        }
        // For intra-function pointers (name contains "+"), check if any
        // BL instruction within ±128 bytes targets an allowed function.
        let nearby_match = name.contains('+') && has_nearby_allowed_callee(
            &elf, text, *target, &nm_map, &annotations.allowed_vtables,
        );
        if !nearby_match {
            violations.push((sec.clone(), *offset, *target, name));
        }
    }

    println!(
        "  {} code pointer(s) in data, {} allowed, {} violation(s)",
        hits.len(),
        hits.len() - violations.len(),
        violations.len()
    );
    println!();

    if violations.is_empty() {
        println!("=== Data-to-code check PASSED ===");
    } else {
        eprintln!("=== Data-to-code check FAILED ===");
        eprintln!();
        for (sec, offset, target, name) in &violations {
            eprintln!("  {} @ {:#x} -> {:#x} ({})", sec, offset, target, name);
        }
        eprintln!();
        eprintln!(
            "Add to [allowed_vtables] in {} if unavoidable.",
            ANNOTATIONS_PATH
        );
        process::exit(1);
    }
}

/// Check if any BL instruction within ±128 bytes of `target_addr` calls a
/// function matching the allowlist. This handles code pointers generated by
/// kprintln! infrastructure: the compiler stores pointers into the calling
/// function's body (for format args / panic locations), and these shift when
/// unrelated code changes. By checking nearby callees we make the allowlist
/// entry for e.g. `uart::Uart>::puts` transitively cover all callers.
fn has_nearby_allowed_callee(
    elf: &[u8],
    text: &ElfSection,
    target_addr: u64,
    nm_map: &HashMap<u64, String>,
    allowed: &HashMap<String, String>,
) -> bool {
    let text_start = text.file_offset;
    let text_vaddr = text.vaddr;
    if target_addr < text_vaddr || target_addr >= text_vaddr + text.size as u64 {
        return false;
    }
    let offset_in_text = (target_addr - text_vaddr) as usize;
    let scan_start = offset_in_text.saturating_sub(128) & !3; // align to 4 bytes
    let scan_end = (offset_in_text + 128).min(text.size) & !3;
    let text_bytes = &elf[text_start..text_start + text.size];

    let mut off = scan_start;
    while off + 4 <= scan_end {
        let insn = u32::from_le_bytes(text_bytes[off..off + 4].try_into().unwrap());
        // AArch64 BL: top 6 bits = 100101 (opcode 0x94-0x97 in top byte)
        if (insn >> 26) == 0b100101 {
            // 26-bit signed immediate, in units of 4 bytes
            let imm26 = (insn & 0x03FF_FFFF) as i64;
            let imm26 = if imm26 & (1 << 25) != 0 {
                imm26 | !0x03FF_FFFF // sign extend
            } else {
                imm26
            };
            let bl_target = (text_vaddr as i64 + off as i64 + imm26 * 4) as u64;
            if let Some(sym_name) = nm_map.get(&bl_target) {
                let is_allowed = allowed.iter().any(|(pattern, _)| {
                    // Strip "+" suffix used for intra-function matching
                    let base = pattern.trim_end_matches('+');
                    sym_name.contains(base)
                });
                if is_allowed {
                    return true;
                }
            }
        }
        off += 4;
    }
    false
}

// ---------------------------------------------------------------------------
// Minimal ELF64 section parser
// ---------------------------------------------------------------------------

struct ElfSection {
    name: String,
    vaddr: u64,
    file_offset: usize,
    size: usize,
}

/// Parse ELF64 section headers. Only extracts name, vaddr, file offset,
/// and size — enough to locate .text and data sections for scanning.
fn parse_elf_sections(elf: &[u8]) -> Vec<ElfSection> {
    // ELF64 header offsets (little-endian)
    let e_shoff = u64::from_le_bytes(elf[40..48].try_into().unwrap()) as usize;
    let e_shentsize = u16::from_le_bytes(elf[58..60].try_into().unwrap()) as usize;
    let e_shnum = u16::from_le_bytes(elf[60..62].try_into().unwrap()) as usize;
    let e_shstrndx = u16::from_le_bytes(elf[62..64].try_into().unwrap()) as usize;

    // Locate .shstrtab (section header string table) for name resolution
    let shstr_hdr = e_shoff + e_shstrndx * e_shentsize;
    let shstr_off =
        u64::from_le_bytes(elf[shstr_hdr + 24..shstr_hdr + 32].try_into().unwrap()) as usize;
    let shstr_size =
        u64::from_le_bytes(elf[shstr_hdr + 32..shstr_hdr + 40].try_into().unwrap()) as usize;
    let shstrtab = &elf[shstr_off..shstr_off + shstr_size];

    let mut sections = Vec::new();
    for i in 0..e_shnum {
        let base = e_shoff + i * e_shentsize;
        let name_off =
            u32::from_le_bytes(elf[base..base + 4].try_into().unwrap()) as usize;
        let vaddr = u64::from_le_bytes(elf[base + 16..base + 24].try_into().unwrap());
        let file_offset =
            u64::from_le_bytes(elf[base + 24..base + 32].try_into().unwrap()) as usize;
        let size =
            u64::from_le_bytes(elf[base + 32..base + 40].try_into().unwrap()) as usize;
        sections.push(ElfSection {
            name: read_cstr(shstrtab, name_off),
            vaddr,
            file_offset,
            size,
        });
    }
    sections
}

/// Read a NUL-terminated string from a byte slice at the given offset.
fn read_cstr(data: &[u8], offset: usize) -> String {
    let start = &data[offset..];
    let len = start.iter().position(|&b| b == 0).unwrap_or(start.len());
    String::from_utf8_lossy(&start[..len]).to_string()
}

// ---------------------------------------------------------------------------
// Symbol map for code pointer resolution
// ---------------------------------------------------------------------------

/// Build an address→name map from `rust-nm --defined-only --demangle`.
fn build_symbol_map(elf_path: &str) -> HashMap<u64, String> {
    let output = Command::new("rust-nm")
        .args(["--defined-only", "--demangle", elf_path])
        .output()
        .expect("failed to run rust-nm — is cargo-binutils installed?");

    let mut map = HashMap::new();
    if !output.status.success() {
        eprintln!("WARN: rust-nm failed, code pointers will show as <unknown>");
        return map;
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // Format: "00000000400801a0 T symbol_name"
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() >= 3 {
            if let Ok(addr) = u64::from_str_radix(parts[0], 16) {
                map.insert(addr, parts[2].to_string());
            }
        }
    }
    map
}

/// Resolve a code pointer to the best matching symbol name.
/// Tries exact match first, then finds the nearest preceding symbol.
fn resolve_code_pointer(addr: u64, nm_map: &HashMap<u64, String>) -> String {
    // Exact match
    if let Some(name) = nm_map.get(&addr) {
        return name.clone();
    }
    // Nearest preceding symbol (addr is inside a function body)
    let mut best_addr = 0u64;
    let mut best_name = None;
    for (&sym_addr, name) in nm_map {
        if sym_addr <= addr && sym_addr > best_addr {
            best_addr = sym_addr;
            best_name = Some(name);
        }
    }
    match best_name {
        Some(name) => format!("{}+{:#x}", name, addr - best_addr),
        None => format!("<unknown @ {:#x}>", addr),
    }
}

// ---------------------------------------------------------------------------
// Pointer cast safety lint
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Init size check — verifies init ELF fits in the kernel mapping buffer
// ---------------------------------------------------------------------------

/// The kernel allocates this many contiguous pages for the init mapping buffer.
/// Must match MAP_BUF_PAGES in src/main.rs.
const INIT_MAP_BUF_PAGES: usize = 16;

/// Mapping struct is 24 bytes (u64 + PhysAddr(u64) + bool + bool + padding).
const MAPPING_SIZE: usize = 24;

/// Page size (4 KiB).
const PAGE_SIZE: u64 = 4096;

/// Stack pages allocated for init by the kernel.
const INIT_STACK_PAGES: usize = 8;

fn check_init_size() {
    let init_elf_path = "user/init/target/aarch64-unknown-none/release/lockjaw-init";
    let elf = match std::fs::read(init_elf_path) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("FAIL: cannot read {}: {}", init_elf_path, e);
            eprintln!("      Run 'make build-user' first.");
            process::exit(1);
        }
    };

    if elf.len() < 64 || &elf[..4] != b"\x7fELF" {
        eprintln!("FAIL: {} is not a valid ELF file", init_elf_path);
        process::exit(1);
    }

    // Parse ELF64 program headers to find PT_LOAD segments
    let e_phoff = u64::from_le_bytes(elf[32..40].try_into().unwrap()) as usize;
    let e_phentsize = u16::from_le_bytes(elf[54..56].try_into().unwrap()) as usize;
    let e_phnum = u16::from_le_bytes(elf[56..58].try_into().unwrap()) as usize;

    let mappings_per_page = PAGE_SIZE as usize / MAPPING_SIZE;
    let capacity = INIT_MAP_BUF_PAGES * mappings_per_page;

    let mut total_pages: usize = 0;
    let mut segment_count = 0;
    // Track distinct 2MB L2 regions — create_address_space has a fixed
    // MAX_L3_TABLES (8) array for caching L3 page table pointers.
    let mut l2_regions: HashSet<u64> = HashSet::new();

    println!("=== Init ELF Mapping Budget ===");
    println!("  Buffer: {} pages = {} mapping slots", INIT_MAP_BUF_PAGES, capacity);
    println!();

    for i in 0..e_phnum {
        let base = e_phoff + i * e_phentsize;
        let p_type = u32::from_le_bytes(elf[base..base + 4].try_into().unwrap());
        // PT_LOAD = 1
        if p_type != 1 {
            continue;
        }
        let p_vaddr = u64::from_le_bytes(elf[base + 16..base + 24].try_into().unwrap());
        let p_memsz = u64::from_le_bytes(elf[base + 40..base + 48].try_into().unwrap());
        let p_flags = u32::from_le_bytes(elf[base + 4..base + 8].try_into().unwrap());
        let pages = ((p_memsz + PAGE_SIZE - 1) / PAGE_SIZE) as usize;
        total_pages += pages;
        segment_count += 1;

        // Record which 2MB L2 regions this segment spans
        let va_start = p_vaddr;
        let va_end = p_vaddr + (pages as u64) * PAGE_SIZE;
        let mut va = va_start & !(2 * 1024 * 1024 - 1); // align down to 2MB
        while va < va_end {
            l2_regions.insert(va >> 21); // L2 index
            va += 2 * 1024 * 1024;
        }

        let r = if p_flags & 4 != 0 { "R" } else { "-" };
        let w = if p_flags & 2 != 0 { "W" } else { "-" };
        let x = if p_flags & 1 != 0 { "X" } else { "-" };
        println!("  Segment {}: VA {:#010x}, {} pages ({} KiB) [{}{}{}]",
            segment_count - 1, p_vaddr, pages, pages * 4, r, w, x);
    }

    // Stack at USER_STACK_BASE (0x800000) also consumes L2 regions
    let stack_base: u64 = 0x800000;
    let stack_end = stack_base + (INIT_STACK_PAGES as u64) * PAGE_SIZE;
    let mut va = stack_base & !(2 * 1024 * 1024 - 1);
    while va < stack_end {
        l2_regions.insert(va >> 21);
        va += 2 * 1024 * 1024;
    }

    let total_with_stack = total_pages + INIT_STACK_PAGES;
    let used_pct = (total_with_stack * 100) / capacity;
    const MAX_L3_TABLES: usize = 8;

    println!();
    println!("  ELF segments:  {} pages", total_pages);
    println!("  Stack:         {} pages", INIT_STACK_PAGES);
    println!("  Total:         {} / {} mapping slots ({}%)", total_with_stack, capacity, used_pct);
    println!("  L2 regions:    {} / {} (each covers 2 MiB of VA space)", l2_regions.len(), MAX_L3_TABLES);

    let mut failed = false;

    if total_with_stack > capacity {
        eprintln!();
        eprintln!("FAIL: init ELF needs {} mappings but buffer holds {}.", total_with_stack, capacity);
        eprintln!("      Increase MAP_BUF_PAGES in src/main.rs and INIT_MAP_BUF_PAGES in xtask.");
        failed = true;
    }

    if l2_regions.len() > MAX_L3_TABLES {
        eprintln!();
        eprintln!("FAIL: init spans {} L2 regions but create_address_space supports {}.",
            l2_regions.len(), MAX_L3_TABLES);
        eprintln!("      Increase MAX_L3_TABLES in src/arch/aarch64/vmem.rs.");
        failed = true;
    }

    if failed {
        process::exit(1);
    }

    if used_pct >= 80 {
        println!();
        println!("  WARNING: {}% of mapping buffer used — approaching limit.", used_pct);
    }

    if l2_regions.len() * 100 / MAX_L3_TABLES >= 80 {
        println!();
        println!("  WARNING: {}% of L2 region slots used — approaching limit.", l2_regions.len() * 100 / MAX_L3_TABLES);
    }

    println!();
    println!("  OK");
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

// ---------------------------------------------------------------------------
// check-linker-symbols
// ---------------------------------------------------------------------------

/// Enforce that every linker-symbol-to-integer site in `src/` is
/// classified in `docs/linker-symbol-audit.md`. Catches the
/// regression class where a future PR adds a `&__symbol as u64`
/// (or a function-pointer cast for `_secondary_start`) that
/// silently feeds a PA consumer and breaks after the kernel relink.
///
/// Sites are matched by (file, line) against entries parsed from
/// the audit doc's `| Line | ... |` Markdown table rows. Any source
/// site without a corresponding doc entry — or any doc entry whose
/// line does not match a real source site — fails CI.
fn check_linker_symbols() {
    use std::path::Path;

    println!("=== Linker-Symbol Audit Check ===");
    println!("  Every &__symbol / &raw const __symbol / fn-ptr cast in src/");
    println!("  must be classified in docs/linker-symbol-audit.md.");
    println!();

    let src_dir = Path::new("src");
    if !src_dir.exists() {
        eprintln!("ERROR: src/ directory not found (run from project root)");
        process::exit(1);
    }
    let audit_path = Path::new("docs/linker-symbol-audit.md");
    if !audit_path.exists() {
        eprintln!("ERROR: docs/linker-symbol-audit.md not found");
        process::exit(1);
    }

    // Collect source sites.
    let mut source_sites: HashSet<(String, usize)> = HashSet::new();
    visit_rs_files(src_dir, &mut |path| {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let rel_path = path.to_string_lossy().to_string();
        for (i, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("*") {
                continue;
            }
            if line_has_linker_symbol_site(line) {
                source_sites.insert((rel_path.clone(), i + 1));
            }
        }
    });

    // Parse audit doc. Entries are "| <line> | <classification> |
    // ..." where the file is named in a "### `path`" heading
    // immediately above the table.
    let audit = std::fs::read_to_string(audit_path).expect("read audit doc");
    let mut audited_sites: HashSet<(String, usize)> = HashSet::new();
    let mut current_file: Option<String> = None;
    for line in audit.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("### ") {
            // Heading: extract the path between backticks.
            if let Some(start) = rest.find('`') {
                if let Some(end) = rest[start + 1..].find('`') {
                    current_file = Some(rest[start + 1..start + 1 + end].to_string());
                    continue;
                }
            }
        }
        if let (Some(file), Some(rest)) = (current_file.as_ref(), trimmed.strip_prefix("|")) {
            // Markdown table row: |line|class|...|
            let cells: Vec<&str> = rest.split('|').map(str::trim).collect();
            if let Some(line_str) = cells.first() {
                if let Ok(n) = line_str.parse::<usize>() {
                    audited_sites.insert((file.clone(), n));
                }
            }
        }
    }

    println!("  source sites found: {}", source_sites.len());
    println!("  audited sites listed: {}", audited_sites.len());
    println!();

    let mut missing: Vec<&(String, usize)> = source_sites.difference(&audited_sites).collect();
    let mut stale: Vec<&(String, usize)> = audited_sites.difference(&source_sites).collect();
    missing.sort();
    stale.sort();

    if missing.is_empty() && stale.is_empty() {
        println!("=== Linker-symbol audit check PASSED ===");
        return;
    }

    eprintln!("=== Linker-symbol audit check FAILED ===");
    if !missing.is_empty() {
        eprintln!();
        eprintln!("Source sites missing from docs/linker-symbol-audit.md:");
        for (f, l) in &missing {
            eprintln!("  {}:{}", f, l);
        }
    }
    if !stale.is_empty() {
        eprintln!();
        eprintln!("Audit entries with no matching source site:");
        for (f, l) in &stale {
            eprintln!("  {}:{}", f, l);
        }
    }
    eprintln!();
    eprintln!("Add a row in the right table of docs/linker-symbol-audit.md");
    eprintln!("for any new linker-symbol-to-integer site, classified as");
    eprintln!("VA-image / PA / PA-prepivot-static / DISPLAY.");
    process::exit(1);
}

/// True if a line takes the address of something that resolves to a
/// linker-defined kernel-image symbol — `&__sym`, `&raw const __sym`,
/// `&raw const STATIC` (kernel boot statics), or a function-pointer
/// cast that yields a symbol address (`fn as *const () as u64`).
///
/// Conservative: any false positive is harmless (an extra audit row);
/// any false negative is the dangerous case we're guarding against.
fn line_has_linker_symbol_site(line: &str) -> bool {
    // Linker symbols: &__name or &raw const __name
    if line.contains("&__") || line.contains("&raw const __") {
        return true;
    }
    // Kernel boot statics in mmu.rs: &raw const BOOT_/KERNEL_
    if line.contains("&raw const BOOT_") || line.contains("&raw const KERNEL_") {
        return true;
    }
    // Function-pointer cast to integer (PSCI _secondary_start, etc.).
    // Matches `<thing> as *const () as u64`. Narrow enough to skip
    // the common `as *mut T` / `as *const T` casts caught elsewhere.
    if line.contains(" as *const () as u64") {
        return true;
    }
    false
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
