# Paired Review Workflow

Instructions to future-me (Claude Code, main-agent role) for running the
paired codex + opus review loop on staged changes. Established during
the Phase 9/10 typed-MMIO substrate effort (commits `747c5fb`…`653c1f5`,
2026-05-28→05-30). Read this before launching reviews; it captures the
shape Ben asked for and the gotchas that wasted time on the first pass.

## The division of labor

**You stage. Reviewers review. You don't self-review.** Ben said it
directly:

> "the review is staged by you and done by other agents, you don't
> review it yourself in the typical flow."

> "Normally I ask codex to do reviews. If you want to take on the
> burden of spawning subagents for review, then you need to do it
> correctly."

Your job is: stage the diff, launch both reviewers, read the verdicts,
fix findings, re-stage, iterate, commit. You don't write a
"verdict-from-me" between the reviewers' verdicts and the commit.

The reviewer instructions live in two files (already committed):

- `docs/process/reviewer-mode.md` — the opus / Claude reviewer's operating
  instructions. The opus agent reads this; **you don't**. Ben was
  explicit: "The reviewer-mode is for the reviewer to read, not you.
  Don't pollute yourself with the wrong context."
- `docs/process/reviewer-mode-codex.md` — codex's version of the same.
  Reference it from codex prompts, don't read it yourself either.

You should know the files exist and where to find them. You should not
internalize their content as your own reviewing instructions.

## Before a review round

1. Stage the diff: `git add <specific files>`. Don't use `git add -A` or
   `git add .` — staging by name keeps the change set legible and avoids
   accidentally including secrets, image artifacts, or untracked
   experiments.
2. Confirm the staged set: `git diff --cached --stat`.
3. Make sure `make test` is green on the staged tree, OR note explicitly
   why it can't validate the change (e.g. emmc2's DMA path only runs on
   Pi flash). Reviewers ask about validation; have the answer ready.
4. **Never chain `git add && git commit`** (feedback memory:
   `feedback_separate_stage_commit`). Stage, review, commit are three
   separate steps.

## Launching codex (resumable thread)

The shape is `codex exec resume <thread-id> "<prompt>"`, in the
background, output to a file:

```bash
codex exec resume 019e722e-362a-7813-856b-7c396d50af49 \
  "Round N — what changed, what to verify. ... $(git diff --cached)" \
  > /tmp/codex-<descriptor>.txt 2>&1
```

Run via `Bash` with `run_in_background: true`. The exit code goes to
the same output file via `; echo "codex exit $?"`.

### Thread management

- **Reuse the same thread-id across rounds of the same logical effort.**
  Codex remembers prior rounds — when you tell it "Round 3 — addressed
  your round-2 High by X", it knows what its round-2 High was. This is
  the resumable-context win.
- **Start a new thread** for a logically separate effort. The typical
  shape is `codex exec "..."` (no flag needed); the new session is
  recorded under `~/.codex/sessions/`. To continue it in the next
  round, use `codex exec resume --last "..."` — `--last` picks the
  most recent session for the current cwd, which is the right session
  as long as you stay in the repo root between rounds.
- **Pinning to a specific session ID** (cross-cwd, or after starting
  multiple efforts in parallel) requires capturing the ID from the
  first call. `--json` prints all events as JSONL, including a
  session-start event with the session UUID — grep for it
  (`jq -r 'select(.type=="session_started") | .session_id'`). For
  most workflows `resume --last` is simpler and is what the round-
  to-round pattern below assumes.
- **Don't try `codex review --uncommitted [PROMPT]`**: the CLI rejects
  it as mutually exclusive. Use `codex exec` (with or without resume),
  not `codex review`.

### What to put in the prompt

The Codex-reminder rule in `docs/process/ben_principles.md` Tier 4 #19 is
load-bearing: *every* prompt points at `docs/process/ben_principles.md` and
calls out the relevant tier (most commonly Tier 1 #1 correctness-by-
construction, Tier 3 #14 explicit-init / no-coincidental-defaults,
Tier 2 #6 reach-for-userspace-before-kernel). Without the reminder,
codex defaults to "idiomatic Rust", not Lockjaw's rules.

A working prompt has, in order:
1. A one-line "what landed in this round" header (round N, what was
   addressed since last round if applicable).
2. A paragraph describing what the diff does, in Lockjaw vocabulary
   (driver regime, envelope, push→pull, etc. — codex has the project
   context from prior rounds but a re-orientation paragraph keeps it
   sharp).
3. A pointer to `docs/process/ben_principles.md` and the tier(s) most relevant.
4. The specific questions you want answered (a/b/c lettered, concrete:
   "does X close the prior gap?", "is there a Y evasion?").
5. The staged diff (inline `$(git diff --cached)` for diffs ≲200 lines)
   OR a directive to run `git diff --cached` itself (cleaner for big
   diffs — codex has workspace access).

Backticks and apostrophes in the prompt are safe inside `"..."`; the
shell evaluates `$(...)` but treats backticks-in-strings as literal
when wrapped by the outer quotes. Commit messages need the heredoc
form (next section); review prompts do not.

### Extracting the verdict

`codex exec resume` output is plain text. The verdict comes after the
last `tokens used` marker:

```bash
f=/tmp/codex-<descriptor>.txt
echo "lines: $(wc -l < "$f")"
awk '/^tokens used$/{c++} c>=1' "$f" | tail -80
```

If the verdict is longer than that or buried, just `Read` the file at
the right offset (`tail` of `wc -l`). Codex output files can hit
2000–3000 lines because they inline tool-call outputs (sed, rg) —
reading the whole file is wasteful; use the awk tail.

The codex output also contains its tool calls and intermediate
reasoning. The actual verdict is the final `codex` block (after the
last `tokens used` line), often labeled with `- High:` / `- Medium:` /
`- Low:` bullets and a closing summary.

## Launching opus (Agent tool, claude reviewer)

Use the `Agent` tool with:

- `subagent_type: "claude"` (the catch-all; the specialized review
  agents in the available-agents list are for different purposes).
- `model: "opus"` (Ben said: "Spawn opus with the reviewer mode").
- `run_in_background: true`.

The opus agent is **fresh each round** — it has no memory of prior
rounds. You give it the full context every time.

### Prompt structure

```
You are reviewing a STAGED git diff in the Lockjaw kernel repo at
/Users/Ben/Code/Lockjaw. Do NOT edit any files — review only.

First read docs/process/reviewer-mode.md in full and adopt it as your operating
instructions (it defines your review format and standard). Also read
docs/process/ben_principles.md (especially Tier 1 #1 correctness-by-
construction, Tier 3 #14 explicit init).

Context: <one paragraph: what this commit does in Lockjaw vocabulary>.

Run `git diff --cached` to see the diff, and read <list of files> as
needed.

What changed: <bullet list of the concrete changes>.

Validation done: <make test status, smoke tests, Pi flash if any>.

Per reviewer-mode.md, verify per these questions: (a) … (b) … (c) … .
The dangerous failure here is <false-negative / behavior-drift / etc.>;
focus there.
```

Always include "Do NOT edit any files — review only." Opus has the
full toolset including `Edit`/`Write` and can accidentally drift into
fixing things you wanted only flagged.

### Don't read the agent's transcript

When the Agent tool launches, the response includes an `output_file`
path. The framework's own warning is explicit:

> Do NOT Read or tail this file via the shell tool — it is the full
> sub-agent JSONL transcript and reading it will overflow your context.

The agent's verdict comes back as a single message in the `result`
field of the task-completion notification. **That is what you read.**
The transcript file is for the framework, not for you.

### Reviewer-mode primer

`docs/process/reviewer-mode.md` and its codex sibling are the reviewer's
operating instructions: how to format findings (Fix-now / Tech-debt /
no-finding), what scrutiny level, what stale-comment / behavior-drift
patterns to flag. You don't need to know them — but knowing they exist
explains why the reviewer's output is shaped the way it is. If a future
reviewer ever forgets to follow the format, the fix is in those files,
not in your prompt.

## Iterating rounds

For a single logical effort (e.g. Phase 10 C), expect 1–4 rounds:

1. Round 1: ship the change. Get findings.
2. Address Fix-now findings, re-stage.
3. Round 2: codex resumes the thread (it knows what round-1 said).
   Opus is a fresh agent — re-brief it with "this round addresses
   round-N's findings by X". Both will look for *new* issues, not
   just confirm fixes.
4. Pattern across the Phase 10 C effort: codex caught the gross
   syntax-evasion class; opus caught the next layer (alias / raw-ident
   / macro opacity). The reviewers complement each other; spend rounds
   on both, not just whichever was faster last time.

### When to stop iterating

- Both reviewers return "no correctness findings" or only Tech-debt /
  Low — commit.
- A reviewer's Fix-now is a *direct prescription* you can apply
  literally (e.g. "use `Ident::unraw()`", "trim the comment to X"),
  with unit-test coverage for the fix — commit without re-review.
- A reviewer's Fix-now is a *design call* (e.g. "consider
  restructuring") — re-review after the fix to confirm the design
  choice landed cleanly.

### When to skip the paired review entirely

CLAUDE.md says "Stage for Codex review before committing non-trivial
changes." Things that *are* trivial enough to skip:

- Pure mechanical revert of something a reviewer just asked you to
  remove (the gate the reviewer represents is already satisfied).
- Type-annotation refactor whose make-test-green proves correctness by
  construction (e.g. explicit-origin: every annotation must match its
  constructor, so a green compile = every annotation is right).
- A lint addition whose precondition is already established (e.g.
  adding `#![deny(unsafe_code)]` when the file has zero `unsafe` and
  the compile under deny passes).

Even for these, one light codex resume is a cheap receipt. Skip when
the cycle cost outweighs the receipt's value.

## Commit messages

`git commit -m "..."` dies on apostrophes (odd-count → unbalanced quote)
and is fragile around backticks. Always use the heredoc form with a
quoted delimiter:

```bash
git commit -F - <<'MSG'
Commit subject line (~70 chars).

Body. Apostrophes, backticks, em-dashes — anything — fine in here
because the 'MSG' is single-quoted, so the shell does no expansion.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
MSG
```

The `<<'MSG'` (quoted heredoc terminator) is the load-bearing piece.
Without the quotes, `$(...)` and backticks in the body get evaluated
by the shell and the commit body fills with stderr.

## Smoke-testing an enforcement tool

When the change you're reviewing is itself an enforcement check (an
xtask lint, a new attribute, a build gate), pair the unit tests with a
real-driver smoke test: inject all the violations into one driver
(pl011 is small and survives the round trip), run the check, expect it
to fail with the exact findings you predicted, then revert:

```bash
# 1. Edit to inject violations into user/pl011-driver/src/main.rs.
# 2. Run from repo root (do NOT cd — see "don't cd mid-session" gotcha):
cargo xtask check-driver-unsafe 2>&1; echo "exit=$?"
git checkout user/pl011-driver/src/main.rs
git status --short user/pl011-driver/   # must be empty
```

This catches the end-to-end exit-1 path that unit tests don't (they
test detection primitives in isolation; the smoke test tests the
glue + the `process::exit(1)` propagation). Confirm the file is clean
after revert before moving on. **Don't** forget the revert and
commit the injection.

## Gotchas

- **Combined Bash with multiple `echo` + grep stages sometimes
  truncates output mid-stream.** Symptom: a `git add … && git diff
  --cached --stat` follow-up returns nothing because the `git add`
  silently didn't fire. Re-run the unstaged part separately. Heuristic:
  if you see one section's output and not the section after, assume
  truncation, not silent success.
- **The classifier (auto-mode safety check) can be briefly
  unavailable.** Bash errors with "auto mode cannot determine safety";
  `Read` and `Grep` still work (read-only). Wait a moment and retry, or
  use `Read` on the file you'd have `cat`'d.
- **Never pipe `make test` through `tail`** (feedback memory:
  `feedback_make_test_no_tail`). It hides early-stage build/stack/
  integration failures.
- **Don't `cd` mid-session** (feedback memory: `feedback_no_cd`). Stay
  at repo root. There are 11 `main.rs` files in this tree and
  ambiguous tool-call paths waste cycles.
- **The agent's report ≠ what the agent did.** When a subagent reports
  "wrote both docs", verify the files (`ls`, `wc -l`) and spot-check
  one before commit. The transcript-don't-read rule means you can't
  trace it; trust-but-verify against the file system.

## Where to keep the cycle short

Three places to spend or save cycles:

1. **Prompt quality.** A specific prompt ("check the 5 listed evasion
   shapes against the new visit_path / visit_macro split") gets a
   specific verdict back. A vague prompt ("review this") gets a vague
   verdict and another round.
2. **Diff size at stage.** Smaller commits review faster and each
   round costs less. The session's worst cycles were on the big
   restructure (12 files); the cleanest were the focused fixes (2–3
   files).
3. **Round-count discipline.** Three rounds is normal. Four-plus is a
   sign the design needs a step back, not another round of polish.
   Phase 10 C went to round 4 because each round surfaced a genuinely
   new evasion class — that's load-bearing iteration, not polish.
   Distinguish.

## Where reviewer instructions live (recap)

- `docs/process/reviewer-mode.md` — opus reviewer instructions. **Reviewer reads.**
- `docs/process/reviewer-mode-codex.md` — codex reviewer instructions.
  **Reviewer reads** (you reference from prompts).
- `docs/process/paired-review-workflow.md` — *this file*. The orchestrator's
  side. **You read.**
- `docs/process/ben_principles.md` — both reviewers read. **You point at it from
  every prompt** (Tier 4 #19).
