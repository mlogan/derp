# Working with Claude Code on DVM

This document describes the development workflow and methodology used for building the DVM project with Claude Code.

## Overview

This project uses a structured, phase-based approach to implementation with Claude Code as an implementation agent. The workflow emphasizes:
- **Clear planning before coding**
- **Task-driven development**
- **Persistent progress tracking**
- **Incremental, testable implementation**

---

## Workflow

### 1. Top-Level Requirements (PRD)

**File**: `PRD.md`

The Product Requirements Document defines the overall vision, goals, and requirements for the entire project. It covers:
- Executive summary and use cases
- Technical architecture
- All planned phases and features
- Success criteria
- Non-functional requirements

**Purpose**: Provides the big picture and long-term direction.

**Lifecycle**: Created once at project start, updated as the vision evolves.

---

### 2. Phase Implementation Plans

**Files**: `IMPLEMENTATION_PLAN_PHASE1.md`, `IMPLEMENTATION_PLAN_PHASE2.md`, etc.

Each implementation plan breaks down a specific phase of work into concrete, achievable tasks. A good implementation plan includes:

#### Structure:
- **Overview**: What will be built in this phase
- **Goals**: Specific objectives
- **Non-goals**: What's explicitly deferred
- **Detailed specifications**: Technical details for each component
- **Implementation order**: Step-by-step sequence
- **Acceptance criteria**: How to know when it's done
- **Test cases**: What to test

#### Characteristics:
- **Focused scope**: Covers 1-2 weeks of work maximum
- **Self-contained**: Can be completed independently
- **Testable**: Clear success criteria
- **Practical**: No speculation, just concrete next steps

**Purpose**: Tactical plan for a specific chunk of functionality.

**Lifecycle**: Created at the start of each phase, referenced throughout implementation.

---

### 3. Implementation with Claude Code

#### Starting a New Phase

1. **Review the implementation plan** with Claude Code
2. **Claude creates tasks** using its internal task tracking system:
   ```
   TaskCreate, TaskUpdate, TaskList, TaskGet
   ```
3. **Task structure**:
   - Each task corresponds to a specific deliverable
   - Dependencies are tracked (blockedBy/blocks)
   - Status tracked: pending → in_progress → completed
   - Detailed descriptions with acceptance criteria

#### During Implementation

- **Claude works through tasks** in dependency order
- **You can check progress** at any time:
  ```bash
  /tasks
  ```
- **Tasks are marked** in_progress when started, completed when done
- **All work is tested** as it's implemented
- **Tests are written** alongside implementation (TDD approach)

#### Key Principles

1. **No file I/O until needed**: Start with in-memory tests, add file I/O only when necessary
2. **Test everything**: Every feature gets comprehensive tests
3. **Incremental progress**: Complete one task fully before moving to the next
4. **Avoid over-engineering**: Implement only what's in the plan, no extras
5. **Direct communication**: Claude asks questions when requirements are unclear

---

### 4. Recording Progress

**Files**: `TASKS_PHASE1.md`, `TASKS_PHASE2.md`, etc.

At the end of each implementation session, **create a permanent task tracking file**:

#### Contents:
- ✅ **Completed tasks**: What's been built
- 📋 **Remaining tasks**: What's still pending
- 📊 **Test summary**: How many tests passing
- 🎯 **Success metrics**: Progress against acceptance criteria
- 📁 **Project structure**: Where files are located
- 📝 **Notes**: Key decisions and architectural choices

#### Purpose:
- **Persistence**: Claude Code's internal tasks are session-specific
- **Handoff**: Clear state for resuming work later
- **Documentation**: Record of what was built and why
- **Progress tracking**: Clear view of phase completion

#### When to Create:
- At the end of an implementation session
- When switching between phases
- Before taking a break from the project
- When significant milestones are reached

**Naming Convention**: `TASKS_PHASE{N}.md` corresponds to `IMPLEMENTATION_PLAN_PHASE{N}.md`

---

## File Organization

```
project/
├── PRD.md                          # Product Requirements (vision)
├── IMPLEMENTATION_PLAN_PHASE1.md   # Phase 1 plan (what to build)
├── TASKS_PHASE1.md                 # Phase 1 progress (what was built)
├── IMPLEMENTATION_PLAN_PHASE2.md   # Phase 2 plan
├── TASKS_PHASE2.md                 # Phase 2 progress
├── CLAUDE.md                       # This file (how to work)
└── src/                            # Implementation
```

---

## Example Session Flow

### Starting a New Phase

```
User: "Let's implement Phase 1. Here's IMPLEMENTATION_PLAN_PHASE1.md"

Claude: [Reads plan, creates internal tasks]
        "I've created 21 tasks based on the plan. Ready to begin?"

User: "Yes, begin"

Claude: [Implements tasks sequentially, marking progress]
        [Writes tests for everything]
        [Updates task status as work completes]
```

### Mid-Session Check

```
User: "What's our progress?"

Claude: [Shows TaskList]
        "15/21 tasks complete. Currently working on task #16..."
```

### Ending a Session

```
User: "Let's stop for now. Create the permanent task list."

Claude: [Creates TASKS_PHASE1.md with current status]
        "Task file created. 15 tasks complete, 6 remaining."
```

### Resuming Later

```
User: "Let's continue Phase 1"

Claude: [Reads TASKS_PHASE1.md]
        [Recreates internal tasks for remaining work]
        "Resuming Phase 1. 6 tasks remaining, starting with #16..."
```

---

## Benefits of This Approach

### 1. **Clear Structure**
- Always know what to build next
- No ambiguity about scope
- Easy to estimate progress

### 2. **Persistent State**
- Work survives between sessions
- Easy to hand off to other developers
- Clear history of decisions

### 3. **Testability**
- Every phase has clear acceptance criteria
- Tests written alongside code
- Confidence in completed work

### 4. **Flexibility**
- Can pause/resume at any task boundary
- Can adjust plans between phases
- Can work on multiple phases in parallel

### 5. **Documentation**
- Self-documenting progress
- Clear rationale for decisions
- Easy to onboard new contributors

---

## Git Commit Strategy

### Commit Early and Often

**Philosophy**: Make small, frequent commits throughout implementation rather than one large commit at the end.

#### During Implementation

**Commit after each completed task or logical unit**:
- ✅ After implementing a single module/component
- ✅ After getting a test suite passing
- ✅ After fixing a bug or error
- ✅ After refactoring that doesn't change behavior
- ✅ After adding documentation

**Example timeline**:
```
10:00 - Start task: "Implement lexer"
10:30 - Lexer basic structure done → git commit
11:00 - Lexer tests passing → git commit
11:15 - Fix edge case in lexer → git commit
11:30 - Add lexer documentation → git commit
```

#### Commit Message Format

Use clear, descriptive commit messages:

```bash
git commit -m "$(cat <<'EOF'
[Brief summary of what changed]

[Optional: More detailed explanation]
- Bullet points for multiple changes
- Explain why, not just what
- Reference files or components

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>
EOF
)"
```

#### What Makes a Good Commit

✅ **Good commits**:
- Focused on one logical change
- All tests passing
- All lints passing (run `cargo clippy`)
- Code compiles/runs
- Has a clear message explaining the change
- Can be easily reviewed or reverted

❌ **Avoid**:
- Mixing unrelated changes
- Committing broken code
- Committing code with lint warnings
- Vague messages like "fix stuff" or "updates"
- Waiting until end of session to commit everything

#### End of Session: Organize Commits

At the end of a session, you can organize commits into logical groups:

```bash
# Option 1: Keep all incremental commits (shows work process)
git log --oneline  # Review history

# Option 2: Squash related commits into logical groups
# (Advanced - only if comfortable with git rebase)
```

**Recommendation**: Keep incremental commits during active development. They provide:
- Better history of how problems were solved
- Easier debugging (git bisect)
- Natural checkpoints for reverting
- Clear audit trail

---

## Linting

### Always Fix Lints Before Committing

**All code must pass linting before being committed.** This project uses strict linting to maintain code quality.

#### Rust (Clippy)

Run clippy before every commit:

```bash
cargo clippy
```

The project is configured with `clippy::all` and `clippy::pedantic` warnings enabled. Fix all warnings before committing.

**Important principles:**
- Fix the actual lint, don't suppress it with `#[allow(...)]`
- `#[allow(...)]` directives should be used **very sparingly** and only when:
  - The lint is a false positive
  - There's a documented technical reason the code must be this way
- When in doubt, fix the code rather than suppress the warning

#### C Code

The C code is compiled with strict warnings via the build script:
- `-Wall` - Enable all warnings
- `-Wextra` - Enable extra warnings
- `-Werror` - Treat warnings as errors

Any C code changes must compile without warnings.

#### Pre-Commit Checklist

Before committing, verify:
1. `cargo clippy` produces no warnings (except expected `unsafe_code` notices)
2. `cargo test` passes
3. `cargo build` succeeds

---

## Best Practices

### For Implementation Plans

✅ **Do**:
- Break work into small, testable tasks
- Include concrete acceptance criteria
- Specify file locations and structure
- List what's explicitly NOT in scope
- Include example test cases

❌ **Don't**:
- Make plans too large (>20 tasks)
- Leave requirements ambiguous
- Include speculative "nice to have" features
- Skip test planning

### For Working with Claude

✅ **Do**:
- Review and approve plans before starting
- Let Claude ask questions when unclear
- Trust Claude's architectural decisions (unless you disagree)
- Create permanent task files at session end
- Run `/tasks` to check progress
- Commit frequently as work progresses
- Ask Claude to commit after completing each task

❌ **Don't**:
- Start coding without a plan
- Add features not in the plan
- Skip writing tests
- Forget to save task status
- Wait until the end to make one giant commit

### For Task Tracking

✅ **Do**:
- Create TASKS_{PHASE}.md files at session end
- Include test counts and pass/fail status
- Note any deferred/deleted tasks
- Record architectural decisions

❌ **Don't**:
- Rely only on Claude's internal tasks (they're ephemeral)
- Skip recording partial progress
- Forget to link back to the implementation plan

---

## Tips for Success

1. **Start with a good PRD**: Invest time upfront in clear requirements
2. **Keep phases small**: 1-2 weeks of work maximum per phase
3. **Test everything**: Write tests as you implement
4. **Lint before committing**: Run `cargo clippy` and fix all warnings
5. **Commit frequently**: Make small commits after each logical unit of work
6. **Save progress regularly**: Create task files frequently
7. **Trust the process**: The structure works - stick to it
8. **Ask questions**: If something's unclear, ask before implementing
9. **Iterate**: Plans can be adjusted between phases based on learnings
10. **Review history**: Use `git log` to see incremental progress

---

## DVM Project Status

**Current Phase**: Phase 8 (Complete) - malloc/free, system calls, hello world
- Implementation Plan: `IMPLEMENTATION_PLAN_PHASE8.md`
- Task Tracking: `TASKS_PHASE8.md`
- Tests: 296 passing
- `scripts/bench-quick.sh` for fast regression checks; record numbers from
  a full `cargo bench` only
- `dvm run prog.{c,rs,ll}` compiles through LLVM IR (`src/llvm/`) and runs
  it; `VM::run()` is the ARM64 JIT, `VM::run_rust()` the interpreter. Both
  must stay exactly equivalent (`tests/jit_tests.rs`, `tests/llvm_tests.rs`).
- Programs talk to the VM only through `SYSCALL` (exit, write, read, sbrk,
  mmap, munmap, abort); there is no libc. `malloc` is guest code
  (`runtime/malloc.c`, linked in on demand), never a VM service.

**Previous Phases**: 1 (core VM), 2 (benchmarks and optimizations),
3 (threads, processes, deterministic scheduler), 4 (JIT), 5 (LLVM IR
compiler), 6 (register allocation), 7 (benchmark corpus and performance).
See `TASKS_PHASE{N}.md`.

**Rewrite experiment** (branch `mlogan-rewrite`): the repository and
`supervisor/` run native arm64 Mach-O binaries under a
deterministic baton scheduler. Plan in `IMPLEMENTATION_PLAN_REWRITE.md`,
progress in `TASKS_REWRITE.md`, results in `docs/REWRITE_RESULTS.md`.

**Multi-process runs** (branch `mlogan-multiproc`, plan complete): one
scheduler in shared memory for several guests on virtual hosts, with
virtual pids, pipe and lock readiness waits, a virtual network (stream and
datagram sockets, `poll`/`select`/`kevent`), one virtual clock, and a fixed
`--net-latency` as the seam for a network simulator.
`rewrite run --manifest FILE`. Plan in `IMPLEMENTATION_PLAN_MULTIPROC.md`,
progress and deviations in `TASKS_MULTIPROC.md`, results in
`docs/MULTIPROC_RESULTS.md`, usage in `README.md`. Rewritten
binaries need the supervisor dylib; default-linked guests work.

**Run files, host directories, real programs** (same branch, complete): run
files are YAML, every host gets a fresh directory per run that its path
names are held to, and Homebrew's curl fetches from Python's `http.server`
repeatably. Plan in `IMPLEMENTATION_PLAN_RUNFILE.md`, progress and findings
in `TASKS_RUNFILE.md`.

**Process fault injection** (branch `mlogan-fault-injection`, complete):
seeded crashes in virtual time and run-file restart policies with virtual
downtime. Plan in `IMPLEMENTATION_PLAN_FAULTS.md`, progress, limits and
review results in `TASKS_FAULTS.md`.

**Tokio guest** (branch `mlogan-tokio-kv`, complete): `tests/programs/kv`,
a key-value server and clients on tokio using most of `tokio::sync`, runs
repeatably, also under fault injection. Plan in
`IMPLEMENTATION_PLAN_TOKIO.md`, findings in `TASKS_TOKIO.md`.

**Seeded heap layout** (branch `mlogan-seeded-heap`, complete): where a
guest's heap blocks land, and whether a freed block is reused at once, is
drawn from the seed, so bugs that depend on pointer order can be found and
replayed. Plan in `IMPLEMENTATION_PLAN_HEAP.md`, progress in
`TASKS_HEAP.md`.

**Seed bisection** (branch `mlogan-seed-bisect`, complete): `rewrite
bisect` replays a failing seed with every random stream (schedule, faults,
heap layout, entropy) reseeded at a virtual time and binary-searches for
when the failure was decided. Plan in
`IMPLEMENTATION_PLAN_BISECT.md`, results and limits in `TASKS_BISECT.md`.

**Site minimisation** (branch `mlogan-site-bisect`, complete): `rewrite
suspects` masks switch points at hooked loads and stores (then branches
and calls) until no site can be dropped, and names their source lines.
Plan in `IMPLEMENTATION_PLAN_SUSPECTS.md`, results in `TASKS_SUSPECTS.md`.

**Review of PRs #4 to #10** (branch `mlogan-review-fixes`): findings and
what was done about each in `TASKS_REVIEW2.md`. Since then the guest heap
is a 1 TB region, guests outlive neither the launcher nor a dead lock
owner, the supervisor allocates from its own heap, and the trace and mask
paths reach guests through the shared state, not their environment.

**Next Phase**: Not yet planned

---

## Questions?

This workflow was developed collaboratively between the human developer and Claude Code during the DVM project. It can be adapted for other projects with similar characteristics:
- Complex, multi-phase development
- Need for clear progress tracking
- Collaborative AI-assisted development
- Emphasis on testing and quality

For questions or suggestions about this workflow, refer to the git history or update this document.
