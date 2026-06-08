//! `check-driver-unsafe` — enforce the user-mode driver regime.
//!
//! For every driver crate under `user/*-driver` this asserts three
//! invariants that keep drivers honest (CLAUDE.md: "User-mode drivers
//! consume `lockjaw-userlib`, period"):
//!
//!   1. the crate root (`src/main.rs`) carries `#![deny(unsafe_code)]`;
//!   2. driver source contains zero `allow(unsafe_code)` attributes
//!      anywhere — the lone audited boot-entry allow lives in the
//!      `lockjaw_userlib` macro body, never in driver source;
//!   3. driver source contains no path that walks through the
//!      `syscall` segment of `lockjaw_userlib` — except for leaf
//!      names in the syscall allowlist (`sys_exit`, `sys_debug_puts`).
//!      Because the `lockjaw-userlib` root re-export was trimmed to
//!      that allowlist, the `syscall` module is the only route a
//!      forbidden syscall can reach driver scope, and every shape that
//!      route can take — direct `lockjaw_userlib::syscall::sys_foo`
//!      call, plain or brace use (`use ...::syscall::{sys_foo}`),
//!      brace-with-rename (`{sys_foo as y}`), brace-with-self
//!      (`{self, sys_foo}`), spaced path (`syscall :: sys_foo`),
//!      module alias (`use ...syscall as sc;`), or UFCS qself
//!      (`<lockjaw_userlib::syscall>::sys_foo(...)`) — has the
//!      `syscall` ident as a path segment in the AST. The check walks
//!      it with `syn::visit::Visit`, so all of those shapes are
//!      caught by construction.
//!
//! All three checks use `syn` (host-only build-tool dep, never linked
//! into kernel or driver binaries) so syntax variants — spaced attrs,
//! list-form `allow`/`deny`, brace-group uses, line-broken paths,
//! turbofish, UFCS qself — cannot evade the gate by reformatting.
//!
//! Raw-ident normalization: `r#syscall` is a valid alternate spelling
//! of `syscall` that rustc accepts (the `r#` prefix only matters for
//! actual keywords; `syscall` isn't one). All ident comparisons go
//! through `ident_str`, which strips the `r#` prefix, so the syscall
//! scan can't be evaded by raw spelling and the allowlist correctly
//! accepts `r#sys_exit` as the same name as `sys_exit`.
//!
//! Macro backstop: `syn` does not parse `macro_rules!` rule bodies or
//! macro-invocation token streams as structured `Path` / `ItemUse`
//! nodes, so the AST visitors above would miss a forbidden syscall
//! smuggled through a macro
//! (`macro_rules! m { () => { lockjaw_userlib::syscall::sys_X(1) } }`).
//! `visit_macro` therefore does a token-level scan: any `syscall` ident
//! in a macro's token stream is a finding. Strict but consistent —
//! drivers reach the allowlist (`sys_exit`, `sys_debug_puts`) via the
//! root re-export with bare names, never via `syscall::` in a macro, so
//! the strictness has no current false positives.
//!
//! Limitation (tech-debt): `cfg_attr(cond, allow(unsafe_code))` is not
//! descended into by the allow visitor (the visitor sees a `cfg_attr`
//! attribute, not an `allow` one). No driver writes `cfg_attr` today;
//! if one does, the check needs to parse `cfg_attr`'s nested meta.
//! Also out of scope: a driver smuggling a forbidden syscall via a
//! third-party re-export crate; driver `Cargo.toml` deps are restricted
//! to the in-tree `lockjaw-*` crates by policy, none of which re-export
//! `syscall::sys_*`.

use std::path::{Path, PathBuf};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{Attribute, File};

/// Driver crates whose source the regime governs.
const DRIVER_CRATES: &[&str] = &[
    "user/cprman-driver",
    "user/emmc2-driver",
    "user/ramfb-driver",
    "user/pl011-driver",
    "user/virtio-blk-driver",
];

/// Raw syscall wrappers a driver MAY name directly. Everything else in
/// the `syscall::sys_*` surface must go through a `lockjaw-userlib`
/// abstraction.
const SYSCALL_ALLOWLIST: &[&str] = &["sys_exit", "sys_debug_puts"];

/// Specific `<crate>::<module>` prefixes a driver MAY NOT name (any of:
/// `use crate::module::...`, `crate::module::...` path-qualified, raw-
/// ident form, macro body containing the consecutive pair). Driver
/// source must consume these via a `lockjaw_userlib::<module>`
/// re-export rather than naming the underlying crate directly.
///
/// Today: all five driver-imported `lockjaw_regs::<module>` families
/// are banned — `sdhci`, `pl011`, `cprman`, `fw_cfg`, `virtio_mmio`.
/// Drivers consume the safe surface of each through
/// `lockjaw_userlib::<module>` re-exports.
///
/// `lockjaw_userlib::sdhci` re-exports the safe SDHCI surface
/// (Sdhci, register-field newtypes, operation envelope, init helpers)
/// WITHOUT exposing the gated `__sdhci_internal_mint`, structurally
/// enforcing the R3 ("sanctioned transfer path is the only path")
/// property from P9.11/SdhciCommandInit (O7 of the operation-level
/// construction safety plan).
///
/// `lockjaw_userlib::pl011` re-exports the safe PL011 surface plus
/// deadline-bounded TX (`write_byte_deadline`), write-replace IMSC
/// (`set_interrupt_masks`), and the FIFO-drain helper
/// (`drain_rx_fifo`) — pl011 framework-mediation plan P1-P4.
///
/// `lockjaw_userlib::cprman`, `lockjaw_userlib::fwcfg`, and the
/// `VirtioMmio` re-export in `lockjaw_userlib::virtio` cover the
/// remaining three families' driver-side surfaces. Phase B of the
/// rename + regime extension plan added the ban entries; the
/// virtio-blk-driver source was already structurally clean (consumed
/// through `lockjaw_userlib::virtio::*`).
const BANNED_DRIVER_MODULE_PATHS: &[(&str, &str)] = &[
    ("lockjaw_regs", "sdhci"),
    ("lockjaw_regs", "pl011"),
    ("lockjaw_regs", "cprman"),
    ("lockjaw_regs", "fw_cfg"),
    ("lockjaw_regs", "virtio_mmio"),
];

pub fn run() {
    println!("=== Driver-unsafe regime check ===");
    println!(
        "  invariant: every user/*-driver crate is #![deny(unsafe_code)]\n  \
         with zero allow(unsafe_code) attributes, no `syscall::*`\n  \
         path reference beyond {SYSCALL_ALLOWLIST:?}, and no reference\n  \
         to {BANNED_DRIVER_MODULE_PATHS:?} via any path/use/macro\n  \
         shape — drivers consume lockjaw-userlib."
    );

    let mut findings: Vec<String> = Vec::new();
    for crate_dir in DRIVER_CRATES {
        check_crate(crate_dir, &mut findings);
    }

    if findings.is_empty() {
        println!("  {} driver crates clean.", DRIVER_CRATES.len());
        println!("=== Driver-unsafe regime check PASSED ===");
    } else {
        eprintln!("=== Driver-unsafe regime check FAILED ===");
        for f in &findings {
            eprintln!("  - {f}");
        }
        std::process::exit(1);
    }
}

fn check_crate(crate_dir: &str, findings: &mut Vec<String>) {
    let root_path = Path::new(crate_dir).join("src/main.rs");
    let src_dir = Path::new(crate_dir).join("src");
    let files = collect_rs_files(&src_dir);

    // (1) The crate root carries an inner #![deny(unsafe_code)]. The
    // inner-style gate rejects the codex-flagged evasion where an outer
    // #[deny(unsafe_code)] on a helper item gets read as crate-level.
    match parse_file(&root_path) {
        Ok(file) => {
            if !has_inner_deny_unsafe_code(&file.attrs) {
                findings.push(format!(
                    "{}: crate root is missing #![deny(unsafe_code)]",
                    root_path.display(),
                ));
            }
        }
        Err(msg) => {
            findings.push(format!("{}: parse error: {msg}", root_path.display()));
            return;
        }
    }

    // (2) + (3) over every .rs file: walk attributes for allow attrs,
    // walk use trees and paths for syscall references.
    for file_path in &files {
        let file = match parse_file(file_path) {
            Ok(f) => f,
            Err(msg) => {
                findings.push(format!("{}: parse error: {msg}", file_path.display()));
                continue;
            }
        };
        let mut v = DriverVisitor {
            file_path,
            findings: Vec::new(),
        };
        v.visit_file(&file);
        findings.extend(v.findings);
    }
}

/// Parse `path` into a [`syn::File`].
fn parse_file(path: &Path) -> Result<File, String> {
    let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    syn::parse_file(&src).map_err(|e| format!("syn: {e}"))
}

/// True iff `a` is an attribute of the form `name(lint_name)` /
/// `name(..., lint_name, ...)` — a lint attribute mentioning
/// `lint_name`. Tolerates spaced and list-form via syn's structured
/// `parse_nested_meta`.
fn attr_matches_lint(a: &Attribute, name: &str, lint_name: &str) -> bool {
    if !a.path().is_ident(name) {
        return false;
    }
    let mut matched = false;
    let _ = a.parse_nested_meta(|meta| {
        if meta.path.is_ident(lint_name) {
            matched = true;
        }
        Ok(())
    });
    matched
}

/// Crate-level `#![deny(unsafe_code)]`. `syn::File::attrs` are inner
/// attributes by construction, but we still gate on `AttrStyle::Inner`
/// defensively — an outer `#[deny(unsafe_code)]` on a helper item must
/// NOT count as crate-level enforcement (codex's prior High #1).
fn has_inner_deny_unsafe_code(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        matches!(a.style, syn::AttrStyle::Inner(_))
            && attr_matches_lint(a, "deny", "unsafe_code")
    })
}

/// Combined per-file visitor: flags every `allow(unsafe_code)` attribute
/// (check 2) and every path that references the `syscall` module with a
/// non-allowlisted leaf (check 3).
struct DriverVisitor<'a> {
    file_path: &'a Path,
    findings: Vec<String>,
}

impl<'a, 'ast> Visit<'ast> for DriverVisitor<'a> {
    fn visit_attribute(&mut self, a: &'ast Attribute) {
        if attr_matches_lint(a, "allow", "unsafe_code") {
            let line = a.span().start().line;
            self.findings.push(format!(
                "{}:{line}: driver source contains an allow(unsafe_code) attribute",
                self.file_path.display(),
            ));
        }
        syn::visit::visit_attribute(self, a);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        let line = item.span().start().line;
        let mut leaves: Vec<UseLeaf> = Vec::new();
        walk_use_tree(&item.tree, Vec::new(), &mut leaves);
        for leaf in leaves {
            // `syscall` either appears in the path-to-leaf or IS the
            // leaf-name (the `use ...syscall as sc` / `{self}` cases).
            let touches_syscall =
                leaf.path.iter().any(|s| s == "syscall") || leaf.leaf == "syscall";
            if touches_syscall && !SYSCALL_ALLOWLIST.contains(&leaf.leaf.as_str()) {
                self.findings.push(format!(
                    "{}:{line}: driver use statement references the `syscall` module \
                     (imports `{}`; allowed leaf names: {SYSCALL_ALLOWLIST:?})",
                    self.file_path.display(),
                    leaf.leaf,
                ));
            }
            // Banned-module-path check (O7): walk the leaf's full
            // path (prefix segments + leaf-name) for any consecutive
            // (crate, module) pair matching BANNED_DRIVER_MODULE_PATHS.
            // E.g. `use lockjaw_regs::sdhci::Sdhci;` has segments
            // `[lockjaw_regs, sdhci, Sdhci]` and the pair `(lockjaw_regs,
            // sdhci)` is banned.
            //
            // Bare-crate alias also banned: `use lockjaw_regs as lr;`
            // has `full = [lockjaw_regs]` (the rename leaves the
            // original ident in the leaf-name). Without this check, a
            // driver could alias the crate root and then write
            // `lr::sdhci::Foo` — visit_path sees `lr`, not the banned
            // pair, so the pair-match misses entirely. Banning every
            // use-statement that names a banned-pair crate by name
            // closes that alias-bypass (codex round-1 Fix-now).
            let full: Vec<&str> = leaf
                .path
                .iter()
                .chain(core::iter::once(&leaf.leaf))
                .map(|s| s.as_str())
                .collect();
            if let Some(banned) = first_banned_pair(&full) {
                self.findings.push(format!(
                    "{}:{line}: driver use statement references banned module path \
                     `{}::{}` (full leaf path `{}`). Drivers consume these via \
                     `lockjaw_userlib::*` re-exports.",
                    self.file_path.display(),
                    banned.0,
                    banned.1,
                    full.join("::"),
                ));
            } else if full.len() == 1 {
                // Bare-crate-root use: `use lockjaw_regs;` or `use
                // lockjaw_regs as lr;`. The pair-match couldn't fire
                // because there's no second segment to pair with —
                // but the use brings the crate ROOT into scope, and
                // any follow-up `lr::sdhci::Foo` would slip past
                // visit_path's pair-match (it sees `lr`, not
                // `lockjaw_regs`). Block the bare-crate use at the
                // use site instead. Multi-segment uses like `use
                // lockjaw_regs::cprman::Cprman;` are NOT caught here
                // — they're allowed today (non-banned module) and
                // the pair-match owns the banned-module case above.
                if let Some(banned_crate) = full
                    .iter()
                    .find(|s| BANNED_DRIVER_MODULE_PATHS.iter().any(|(c, _)| c == *s))
                {
                    self.findings.push(format!(
                        "{}:{line}: driver use statement names banned-pair crate `{}` \
                         directly (`use {}`). Aliasing the crate root would let \
                         a follow-up path reach a banned module under a different \
                         name; drivers consume via `lockjaw_userlib::*` re-exports \
                         instead.",
                        self.file_path.display(),
                        banned_crate,
                        full.join("::"),
                    ));
                }
            }
        }
        // Don't recurse with the default visitor: we've handled this use
        // tree's leaves. (Default visit_item_use would recurse into
        // visit_path on each UsePath, but `syn::Path` and `syn::UsePath`
        // are different types — visit_path is not actually called on use
        // trees anyway, but skipping the default body keeps the contract
        // explicit.)
        let _ = item;
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        // `extern crate foo;` — `syn::ItemExternCrate` is a distinct
        // AST item from `ItemUse`, so visit_item_use doesn't cover this
        // shape. Module-path bans don't apply to a bare `extern crate
        // foo` (only the crate name is named, not a module path inside
        // it), but if any banned-pair's crate appears as the
        // extern-crate target the import declares the WHOLE crate
        // available — including the banned module. Conservative: flag
        // any extern_crate whose ident matches a banned-pair crate.
        // Raw-ident normalized via ident_str.
        let name = ident_str(&item.ident);
        if BANNED_DRIVER_MODULE_PATHS
            .iter()
            .any(|(crate_name, _)| *crate_name == name)
        {
            let line = item.span().start().line;
            self.findings.push(format!(
                "{}:{line}: driver `extern crate {}` makes a banned module path \
                 reachable (banned-pair crates: {:?}). Drivers consume these via \
                 `lockjaw_userlib::*` re-exports.",
                self.file_path.display(),
                name,
                banned_crate_names(),
            ));
        }
        syn::visit::visit_item_extern_crate(self, item);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segs: Vec<String> = path.segments.iter().map(|s| ident_str(&s.ident)).collect();
        if segs.iter().any(|s| s == "syscall") {
            let last = segs.last().expect("syn::Path has at least one segment");
            if !SYSCALL_ALLOWLIST.contains(&last.as_str()) {
                let line = path.segments.first().unwrap().ident.span().start().line;
                self.findings.push(format!(
                    "{}:{line}: driver path `{}` references the `syscall` module \
                     (allowed leaf names: {SYSCALL_ALLOWLIST:?})",
                    self.file_path.display(),
                    segs.join("::"),
                ));
            }
        }
        // Banned-module-path check (O7): walk path segments for any
        // consecutive (crate, module) pair matching
        // BANNED_DRIVER_MODULE_PATHS. Catches
        // `lockjaw_regs::sdhci::Sdhci::new()` and similar path-
        // qualified references that aren't `use` items.
        let seg_strs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        if let Some(banned) = first_banned_pair(&seg_strs) {
            let line = path.segments.first().unwrap().ident.span().start().line;
            self.findings.push(format!(
                "{}:{line}: driver path `{}` references banned module path \
                 `{}::{}`. Drivers consume these via `lockjaw_userlib::*` \
                 re-exports.",
                self.file_path.display(),
                segs.join("::"),
                banned.0,
                banned.1,
            ));
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        // `syn` doesn't parse `macro_rules!` rule bodies or macro
        // invocation args as structured Path/ItemUse nodes -- they are
        // opaque token streams, so the AST visitors above would miss a
        // syscall path hidden in a macro. Walk the token stream and
        // flag any `syscall` ident: strict, but consistent with the
        // regime (drivers reach the allowlist via the root re-export
        // with bare names, never via `syscall::` in a macro).
        if token_stream_contains_ident(&m.tokens, "syscall") {
            let line = m.span().start().line;
            self.findings.push(format!(
                "{}:{line}: driver macro body / invocation contains the \
                 `syscall` ident -- drivers may not reach the syscall \
                 module via macros (allowed leaf names: {SYSCALL_ALLOWLIST:?})",
                self.file_path.display(),
            ));
        }
        // Banned-module-path check (O7): the same macro-opacity gap
        // applies. The macro body is an opaque token stream, so walk
        // it for any consecutive ident pair matching
        // BANNED_DRIVER_MODULE_PATHS. Token-level "consecutive" means
        // adjacent in the linear token sequence, possibly separated by
        // `::` punctuation — `token_stream_contains_pair` strips
        // punctuation when comparing.
        for (crate_name, module) in BANNED_DRIVER_MODULE_PATHS {
            if token_stream_contains_pair(&m.tokens, crate_name, module) {
                let line = m.span().start().line;
                self.findings.push(format!(
                    "{}:{line}: driver macro body / invocation contains the \
                     `{}::{}` ident pair -- drivers may not reach banned \
                     module paths via macros.",
                    self.file_path.display(),
                    crate_name,
                    module,
                ));
            }
        }
        syn::visit::visit_macro(self, m);
    }
}

/// Find the first `(crate, module)` pair from
/// `BANNED_DRIVER_MODULE_PATHS` that appears as two consecutive
/// segments in `segs`. Returns `None` if no pair matches. Linear
/// scan over `segs.windows(2)` × `BANNED_DRIVER_MODULE_PATHS` —
/// small constant on each side.
fn first_banned_pair(segs: &[&str]) -> Option<(&'static str, &'static str)> {
    for window in segs.windows(2) {
        for (crate_name, module) in BANNED_DRIVER_MODULE_PATHS {
            if window[0] == *crate_name && window[1] == *module {
                return Some((crate_name, module));
            }
        }
    }
    None
}

/// Collect the unique crate names from `BANNED_DRIVER_MODULE_PATHS`
/// (the first element of each pair, deduplicated). Used by the
/// `extern crate` diagnostic.
fn banned_crate_names() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = BANNED_DRIVER_MODULE_PATHS
        .iter()
        .map(|(c, _)| *c)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// One leaf of a use tree: the segments from the use root down to the
/// item being imported, and the leaf identifier (the ORIGINAL name, not
/// any `as` rename target; or `*` for a glob).
struct UseLeaf {
    path: Vec<String>,
    leaf: String,
}

/// Strip the `r#` raw-identifier prefix from an ident's display form
/// so `r#syscall` matches `syscall` and `r#sys_exit` matches the
/// allowlist's `sys_exit`. Raw idents are an alternate spelling rustc
/// accepts for any non-keyword name; without this normalization the
/// syscall scan and the allowlist filter would both be evadable by
/// raw-ident spelling (opus round-3 Fix-now).
fn ident_str(i: &proc_macro2::Ident) -> String {
    let s = i.to_string();
    s.strip_prefix("r#").map(|t| t.to_string()).unwrap_or(s)
}

fn walk_use_tree(tree: &syn::UseTree, prefix: Vec<String>, leaves: &mut Vec<UseLeaf>) {
    match tree {
        syn::UseTree::Path(p) => {
            let mut next = prefix;
            next.push(ident_str(&p.ident));
            walk_use_tree(&p.tree, next, leaves);
        }
        syn::UseTree::Name(n) => {
            push_use_leaf(prefix, ident_str(&n.ident), leaves);
        }
        syn::UseTree::Rename(r) => {
            // `use foo::bar as baz;` — the original imported name is
            // `bar`, regardless of the `baz` rename. That's what the
            // regime cares about.
            push_use_leaf(prefix, ident_str(&r.ident), leaves);
        }
        syn::UseTree::Glob(_) => {
            leaves.push(UseLeaf {
                path: prefix,
                leaf: "*".to_string(),
            });
        }
        syn::UseTree::Group(g) => {
            for item in &g.items {
                walk_use_tree(item, prefix.clone(), leaves);
            }
        }
    }
}

fn push_use_leaf(prefix: Vec<String>, name: String, leaves: &mut Vec<UseLeaf>) {
    if name == "self" {
        // `use foo::{self}` imports `foo` itself. The leaf-name we care
        // about is the parent module name.
        if let Some(parent) = prefix.last().cloned() {
            let mut p = prefix;
            p.pop();
            leaves.push(UseLeaf { path: p, leaf: parent });
        }
    } else {
        leaves.push(UseLeaf { path: prefix, leaf: name });
    }
}

/// Recursively check whether a `TokenStream` contains the identifier
/// `name` (at any depth, including inside grouped tokens like braces
/// / parens / brackets). Used by `visit_macro` to backstop the macro
/// opacity gap -- `syn` does not parse macro token streams into
/// structured `Path` nodes, so a forbidden segment hidden inside a
/// `macro_rules!` body or a macro-invocation arg would slip past the
/// AST visitors above. Raw-ident normalized via `ident_str`.
fn token_stream_contains_ident(ts: &proc_macro2::TokenStream, name: &str) -> bool {
    ts.clone().into_iter().any(|t| match t {
        proc_macro2::TokenTree::Ident(i) => ident_str(&i) == name,
        proc_macro2::TokenTree::Group(g) => token_stream_contains_ident(&g.stream(), name),
        _ => false,
    })
}

/// Recursively check whether a `TokenStream` contains the adjacent
/// ident pair `first :: second` (with `::` punctuation between, since
/// path segments in token streams are separated by Punct(':') Punct(':')).
/// Walks groups too. Raw-ident normalized via `ident_str`.
///
/// Used for banned-module-path detection in macro bodies (O7) —
/// catches `lockjaw_regs::sdhci::Foo` hidden in a `macro_rules!` rule
/// or a macro-invocation arg.
fn token_stream_contains_pair(
    ts: &proc_macro2::TokenStream,
    first: &str,
    second: &str,
) -> bool {
    let flat: Vec<proc_macro2::TokenTree> = ts.clone().into_iter().collect();
    // Look for the sequence: Ident(first), Punct(':' joint), Punct(':' alone),
    // Ident(second). The two-colon `::` lexes as two Punct tokens, the
    // first joint, the second alone.
    let n = flat.len();
    let mut i = 0;
    while i + 3 < n {
        if let (
            proc_macro2::TokenTree::Ident(a),
            proc_macro2::TokenTree::Punct(p1),
            proc_macro2::TokenTree::Punct(p2),
            proc_macro2::TokenTree::Ident(b),
        ) = (&flat[i], &flat[i + 1], &flat[i + 2], &flat[i + 3])
        {
            if ident_str(a) == first
                && p1.as_char() == ':'
                && p2.as_char() == ':'
                && ident_str(b) == second
            {
                return true;
            }
        }
        i += 1;
    }
    // Recurse into grouped tokens (braces / parens / brackets).
    for t in &flat {
        if let proc_macro2::TokenTree::Group(g) = t {
            if token_stream_contains_pair(&g.stream(), first, second) {
                return true;
            }
        }
    }
    false
}

/// All `.rs` files under `dir`, recursively, sorted for stable output.
fn collect_rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_rs_files_into(dir, &mut out);
    out.sort();
    out
}

fn collect_rs_files_into(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files_into(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dummy_path() -> PathBuf { PathBuf::from("test.rs") }

    fn syscall_findings(src: &str) -> Vec<String> {
        let file: File = syn::parse_str(src).expect("valid Rust");
        let path = dummy_path();
        let mut v = DriverVisitor { file_path: &path, findings: Vec::new() };
        v.visit_file(&file);
        v.findings
    }

    // --- syn-based deny check --------------------------------------------

    #[test]
    fn deny_inner_attr_canonical_form() {
        let f: File = syn::parse_str("#![deny(unsafe_code)]\nfn _x() {}").unwrap();
        assert!(has_inner_deny_unsafe_code(&f.attrs));
    }

    #[test]
    fn deny_inner_attr_spaced_form() {
        let f: File = syn::parse_str("#![deny( unsafe_code )]\nfn _x() {}").unwrap();
        assert!(has_inner_deny_unsafe_code(&f.attrs));
    }

    #[test]
    fn deny_inner_attr_list_form() {
        let f: File = syn::parse_str("#![deny(unsafe_code, unused)]\nfn _x() {}").unwrap();
        assert!(has_inner_deny_unsafe_code(&f.attrs));
    }

    #[test]
    fn deny_missing_is_detected() {
        let f: File = syn::parse_str("fn _x() {}").unwrap();
        assert!(!has_inner_deny_unsafe_code(&f.attrs));
    }

    #[test]
    fn deny_outer_on_item_does_not_count_as_crate_level() {
        let f: File = syn::parse_str("#[deny(unsafe_code)]\nfn _x() {}").unwrap();
        assert!(!has_inner_deny_unsafe_code(&f.attrs));
    }

    // --- allow check -----------------------------------------------------

    #[test]
    fn allow_finds_canonical_outer() {
        let f = syscall_findings("#[allow(unsafe_code)]\nfn _f() {}\n");
        assert!(f.iter().any(|s| s.contains("allow(unsafe_code)")), "{f:?}");
    }

    #[test]
    fn allow_finds_spaced_form() {
        let f = syscall_findings("#[allow( unsafe_code )]\nfn _f() {}\n");
        assert!(f.iter().any(|s| s.contains("allow(unsafe_code)")), "{f:?}");
    }

    #[test]
    fn allow_finds_list_form() {
        let f = syscall_findings("#[allow(unsafe_code, unused)]\nfn _f() {}\n");
        assert!(f.iter().any(|s| s.contains("allow(unsafe_code)")), "{f:?}");
    }

    #[test]
    fn allow_ignores_unrelated() {
        let f = syscall_findings("#[allow(unused)]\nfn _f() {}\n");
        assert!(!f.iter().any(|s| s.contains("allow(unsafe_code)")), "{f:?}");
    }

    // --- syscall path check: direct call + use ---------------------------

    #[test]
    fn flags_direct_use_of_forbidden_sys() {
        let f = syscall_findings(
            "use lockjaw_userlib::syscall::sys_alloc_pages;\nfn _f() {}",
        );
        assert!(
            f.iter().any(|s| s.contains("syscall") && s.contains("sys_alloc_pages")),
            "{f:?}"
        );
    }

    #[test]
    fn flags_direct_path_call() {
        let f = syscall_findings(
            "fn _f() { let _ = lockjaw_userlib::syscall::sys_alloc_pages(0); }",
        );
        assert!(f.iter().any(|s| s.contains("sys_alloc_pages")), "{f:?}");
    }

    #[test]
    fn allows_use_of_sys_exit() {
        let f = syscall_findings(
            "use lockjaw_userlib::syscall::sys_exit;\nfn _f() {}",
        );
        assert!(f.is_empty(), "{f:?}");
    }

    // --- the 5 evasion shapes opus listed --------------------------------

    #[test]
    fn flags_brace_group_import() {
        let f = syscall_findings(
            "use lockjaw_userlib::syscall::{sys_alloc_pages};\nfn _f() {}",
        );
        assert!(f.iter().any(|s| s.contains("sys_alloc_pages")), "{f:?}");
    }

    #[test]
    fn flags_brace_with_rename() {
        let f = syscall_findings(
            "use lockjaw_userlib::syscall::{sys_alloc_pages as y};\nfn _f() {}",
        );
        // The original name is `sys_alloc_pages` (the `as y` rename
        // doesn't hide it from the check).
        assert!(f.iter().any(|s| s.contains("sys_alloc_pages")), "{f:?}");
    }

    #[test]
    fn flags_brace_with_self_module_import() {
        // `use ...syscall::{self, sys_X};` — `self` imports the syscall
        // module itself (leaf-name = syscall), AND sys_X is forbidden.
        let f = syscall_findings(
            "use lockjaw_userlib::syscall::{self, sys_alloc_pages};\nfn _f() {}",
        );
        assert!(
            f.iter().any(|s| s.contains("`syscall`")),
            "{f:?} — should flag the self leaf (module import)"
        );
        assert!(
            f.iter().any(|s| s.contains("sys_alloc_pages")),
            "{f:?} — should flag the brace leaf"
        );
    }

    #[test]
    fn flags_spaced_path_in_call() {
        // syn parses through whitespace in paths.
        let f = syscall_findings(
            "fn _f() { let _ = lockjaw_userlib :: syscall :: sys_alloc_pages(0); }",
        );
        assert!(f.iter().any(|s| s.contains("sys_alloc_pages")), "{f:?}");
    }

    #[test]
    fn flags_ufcs_qself_through_syscall() {
        // <lockjaw_userlib::syscall>::sys_alloc_pages(...) — the qself
        // path contains `syscall`; visit_path on the qself path catches
        // it (last segment = `syscall`, not in allowlist).
        let f = syscall_findings(
            "fn _f() { let _ = <lockjaw_userlib::syscall>::sys_alloc_pages(0); }",
        );
        assert!(
            f.iter().any(|s| s.contains("syscall")),
            "{f:?} — UFCS qself path should flag"
        );
    }

    // --- module alias evasion (codex's High #1 in round 2) ---------------

    #[test]
    fn flags_module_alias_use() {
        // `use lockjaw_userlib::syscall as sc;` — the leaf-name is
        // `syscall` (the original), which is not in the allowlist.
        let f = syscall_findings(
            "use lockjaw_userlib::syscall as sc;\nfn _f() {}",
        );
        assert!(
            f.iter().any(|s| s.contains("`syscall`")),
            "{f:?} — module alias must flag at the `use` line"
        );
    }

    // --- non-finding cases -----------------------------------------------

    #[test]
    fn does_not_flag_non_syscall_paths() {
        let f = syscall_findings(
            "use lockjaw_userlib::dma::OwnedDmaMapping;\n\
             fn _f() { let _: lockjaw_userlib::dma::DmaBacking = todo!(); }",
        );
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn does_not_flag_glob_of_non_syscall() {
        let f = syscall_findings("use lockjaw_userlib::dma::*;\nfn _f() {}");
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn flags_glob_of_syscall_module() {
        // `use ...syscall::*;` brings every forbidden item into scope.
        let f = syscall_findings(
            "use lockjaw_userlib::syscall::*;\nfn _f() {}",
        );
        assert!(
            f.iter().any(|s| s.contains("`*`") || s.contains("`syscall`")),
            "{f:?} — syscall::* glob must flag"
        );
    }

    // --- macro opacity backstop (codex round-3 High) ---------------------

    #[test]
    fn flags_macro_rules_with_forbidden_syscall_path() {
        // codex's example: the macro_rules! rule body contains the
        // syscall path. syn doesn't parse the body as a Path, but the
        // token-level scan finds the `syscall` ident inside the rule
        // group.
        let f = syscall_findings(
            "macro_rules! m { () => { lockjaw_userlib::syscall::sys_alloc_pages(1) }; }\n\
             fn _f() { m!(); }",
        );
        assert!(
            f.iter().any(|s| s.contains("macro") && s.contains("syscall")),
            "{f:?} — macro_rules! body must be flagged"
        );
    }

    #[test]
    fn flags_macro_invocation_args_with_syscall() {
        // The args of a macro invocation are an opaque token stream to
        // syn. The token-level scan finds the `syscall` ident inside
        // the invocation group.
        let f = syscall_findings(
            "fn _f() { my_macro!(lockjaw_userlib::syscall::sys_alloc_pages(1)); }",
        );
        assert!(
            f.iter().any(|s| s.contains("macro") && s.contains("syscall")),
            "{f:?} — macro invocation args must be flagged"
        );
    }

    #[test]
    fn does_not_flag_macro_with_bare_allowlist_call() {
        // Drivers reach allowlist syscalls via bare names (from the
        // root re-export), so a macro body calling `sys_exit()` has no
        // `syscall` ident at all — no finding.
        let f = syscall_findings(
            "macro_rules! m { () => { sys_exit() }; }\nfn _f() { m!(); }",
        );
        assert!(f.is_empty(), "{f:?}");
    }

    // --- raw-ident normalization (opus round-3 Fix-now) ------------------

    #[test]
    fn flags_raw_ident_module_in_use() {
        // `r#syscall` resolves to the `syscall` module — must flag.
        let f = syscall_findings(
            "use lockjaw_userlib::r#syscall::sys_alloc_pages;\nfn _f() {}",
        );
        assert!(
            f.iter().any(|s| s.contains("sys_alloc_pages")),
            "{f:?} — r#syscall must be normalized to syscall"
        );
    }

    #[test]
    fn flags_raw_ident_module_in_path_call() {
        let f = syscall_findings(
            "fn _f() { let _ = lockjaw_userlib::r#syscall::sys_alloc_pages(0); }",
        );
        assert!(
            f.iter().any(|s| s.contains("sys_alloc_pages")),
            "{f:?} — r#syscall in expression path must be flagged"
        );
    }

    #[test]
    fn flags_raw_ident_module_in_macro() {
        let f = syscall_findings(
            "macro_rules! m { () => { lockjaw_userlib::r#syscall::sys_x(1) }; }\n\
             fn _f() { m!(); }",
        );
        assert!(
            f.iter().any(|s| s.contains("macro") && s.contains("syscall")),
            "{f:?} — r#syscall inside a macro body must be flagged"
        );
    }

    #[test]
    fn allowlist_accepts_raw_ident_form() {
        // `r#sys_exit` is the same allowlist name as `sys_exit` —
        // accepting it prevents a false-positive on a legitimate
        // fully-qualified allowlist call written with raw syntax.
        let f = syscall_findings(
            "use lockjaw_userlib::syscall::r#sys_exit;\nfn _f() {}",
        );
        assert!(f.is_empty(), "{f:?}");
    }

    // --- banned-module-path ban (O7: lockjaw_regs::sdhci forbidden) -----

    #[test]
    fn flags_banned_module_use_tree() {
        // `use lockjaw_regs::sdhci::Sdhci;` — segments
        // [lockjaw_regs, sdhci, Sdhci] contain the banned (lockjaw_regs,
        // sdhci) pair.
        let f = syscall_findings("use lockjaw_regs::sdhci::Sdhci;\nfn _f() {}");
        assert!(
            f.iter().any(|s| s.contains("banned module path") && s.contains("lockjaw_regs::sdhci")),
            "{f:?}"
        );
    }

    #[test]
    fn flags_banned_module_path_call() {
        let f = syscall_findings(
            "fn _f() { let _ = lockjaw_regs::sdhci::Sdhci::some_method(); }",
        );
        assert!(
            f.iter().any(|s| s.contains("banned module path") && s.contains("lockjaw_regs::sdhci")),
            "{f:?}"
        );
    }

    #[test]
    fn flags_banned_module_raw_ident_crate() {
        // `r#lockjaw_regs::sdhci::Sdhci` — raw-ident on the banned
        // crate; ident_str normalization strips r# before pair-match.
        let f = syscall_findings(
            "use r#lockjaw_regs::sdhci::Sdhci;\nfn _f() {}",
        );
        assert!(
            f.iter().any(|s| s.contains("banned module path") && s.contains("lockjaw_regs::sdhci")),
            "{f:?}"
        );
    }

    #[test]
    fn flags_banned_module_raw_ident_module() {
        // `lockjaw_regs::r#sdhci::Sdhci` — raw-ident on the banned
        // module name.
        let f = syscall_findings(
            "use lockjaw_regs::r#sdhci::Sdhci;\nfn _f() {}",
        );
        assert!(
            f.iter().any(|s| s.contains("banned module path") && s.contains("lockjaw_regs::sdhci")),
            "{f:?}"
        );
    }

    #[test]
    fn flags_extern_crate_banned_crate() {
        // `extern crate lockjaw_regs;` makes the banned module path
        // reachable. visit_item_extern_crate fires on the crate name.
        let f = syscall_findings("extern crate lockjaw_regs;\nfn _f() {}");
        assert!(
            f.iter().any(|s| s.contains("extern crate") && s.contains("lockjaw_regs")),
            "{f:?}"
        );
    }

    #[test]
    fn flags_extern_crate_banned_with_raw_ident() {
        let f = syscall_findings("extern crate r#lockjaw_regs;\nfn _f() {}");
        assert!(
            f.iter().any(|s| s.contains("extern crate") && s.contains("lockjaw_regs")),
            "{f:?}"
        );
    }

    #[test]
    fn flags_banned_module_in_macro() {
        // Macro body containing the banned ident pair — token-stream
        // scan via token_stream_contains_pair.
        let f = syscall_findings(
            "macro_rules! m { () => { lockjaw_regs::sdhci::Sdhci }; }\nfn _f() { m!(); }",
        );
        assert!(
            f.iter().any(|s| s.contains("macro") && s.contains("lockjaw_regs::sdhci")),
            "{f:?}"
        );
    }

    #[test]
    fn allows_lockjaw_userlib_sdhci_re_export() {
        // Drivers consume Sdhci via the lockjaw-userlib re-export.
        // This is the canonical path; must NOT flag.
        let f = syscall_findings(
            "use lockjaw_userlib::sdhci::{Sdhci, SdhciOpToken};\nfn _f() {}",
        );
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn allows_lockjaw_regs_non_banned_module() {
        // `lockjaw_regs::cprman::*` is NOT in the banned-pair list
        // (cprman-driver hasn't been migrated yet). Other drivers can
        // still consume their own lockjaw_regs modules directly.
        // When cprman-driver migrates, the pair gets added here.
        let f = syscall_findings(
            "use lockjaw_regs::cprman::Cprman;\nfn _f() {}",
        );
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn flags_alias_bypass_of_banned_pair_crate() {
        // codex round-1 Fix-now: `use lockjaw_regs as lr;` aliases the
        // crate root, then `lr::sdhci::Foo` would reach the banned
        // module under a different name — visit_path sees `lr`, not
        // `lockjaw_regs`, so the pair-match misses. Block the alias
        // at the `use` site instead.
        let f = syscall_findings("use lockjaw_regs as lr;\nfn _f() {}");
        assert!(
            f.iter().any(|s| s.contains("names banned-pair crate") && s.contains("lockjaw_regs")),
            "{f:?}"
        );
    }

    #[test]
    fn flags_bare_use_of_banned_pair_crate() {
        // `use lockjaw_regs;` (without an alias) is the same shape —
        // brings the crate root into scope and lets later code write
        // `lockjaw_regs::sdhci::Foo`, but visit_item_use's pair-match
        // sees only `["lockjaw_regs"]` (no module segment). Catch it
        // at the `use` site via the bare-crate fallback.
        let f = syscall_findings("use lockjaw_regs;\nfn _f() {}");
        assert!(
            f.iter().any(|s| s.contains("names banned-pair crate") && s.contains("lockjaw_regs")),
            "{f:?}"
        );
    }

    #[test]
    fn flags_bare_use_of_pl011_ban_pair() {
        // pl011 added to BANNED_DRIVER_MODULE_PATHS in P1 of the
        // pl011 framework-mediation plan. The list enumeration in
        // every visitor automatically covers it; this test is the
        // representative confirmation that the new pair is wired.
        let f = syscall_findings("use lockjaw_regs::pl011::Pl011;\nfn _f() {}");
        assert!(
            f.iter().any(|s| s.contains("banned module path") && s.contains("lockjaw_regs::pl011")),
            "{f:?}"
        );
    }

    #[test]
    fn flags_bare_use_of_cprman_ban_pair() {
        // cprman added to BANNED_DRIVER_MODULE_PATHS in Phase B of the
        // rename + regime extension plan. Representative test.
        let f = syscall_findings("use lockjaw_regs::cprman::Cprman;\nfn _f() {}");
        assert!(
            f.iter().any(|s| s.contains("banned module path") && s.contains("lockjaw_regs::cprman")),
            "{f:?}"
        );
    }

    #[test]
    fn flags_bare_use_of_fw_cfg_ban_pair() {
        // fw_cfg added to BANNED_DRIVER_MODULE_PATHS in Phase B of the
        // rename + regime extension plan. Representative test.
        let f = syscall_findings("use lockjaw_regs::fw_cfg::FwCfg;\nfn _f() {}");
        assert!(
            f.iter().any(|s| s.contains("banned module path") && s.contains("lockjaw_regs::fw_cfg")),
            "{f:?}"
        );
    }

    #[test]
    fn flags_bare_use_of_virtio_mmio_ban_pair() {
        // virtio_mmio added to BANNED_DRIVER_MODULE_PATHS in Phase B of
        // the rename + regime extension plan. virtio-blk-driver was
        // already structurally clean (consumes through
        // lockjaw_userlib::virtio::*); the ban entry locks in the
        // existing property. Representative test.
        let f = syscall_findings("use lockjaw_regs::virtio_mmio::VirtioMmio;\nfn _f() {}");
        assert!(
            f.iter().any(|s| s.contains("banned module path") && s.contains("lockjaw_regs::virtio_mmio")),
            "{f:?}"
        );
    }
}
