# Development Methodology

Development methodology for the AI agent.

## Principles

### 1. Measure before optimizing
Before optimizing — profile. Without data on bottlenecks, any optimization is guessing.

### 2. Systematic decomposition
Complex problems break into measurable phases with success criteria. Each phase is a verifiable step.

### 3. Hypotheses before action
Before every significant change, formulate a hypothesis: what we expect, how we verify, what we do if it doesn't work.

### 4. Code is responsibility
Written code must work. Not "probably works", but verified to work.

### 5. Fail fast, recover gracefully
Errors should be detected as early as possible. Error handling is part of design, not a patch.

### 6. Backward compatibility
Public API changes must be backward-compatible or explicitly migrated.

### 7. Language rule
ALWAYS respond in the same language the user writes in.

## Workflow

### Task Analysis
1. `pop_notifications` — check pending notifications
2. `get_active_task` — understand current context
3. `search` — find relevant information in KB
4. Define acceptance criteria
5. Break into subtasks
6. Assess risks (what could break)

### Implementation
1. Solve one subtask at a time
2. Verify after each step (compilation, tests, logic)
3. Save intermediate results to the active task
4. `update_context` after significant steps

### Verification
1. Code compiles without warnings
2. Tests pass
3. Logic matches acceptance criteria
4. No side effects on adjacent code
5. Verification checklist passed (see core_ai_behavior.md)

## Task Creation

### Principle: large tasks with deep decomposition

One task = one line of work. Do not create many small tasks — merge them into large ones with internal structure.

**Bad:** 10 tasks "fix bug X", "add feature Y", "refactor Z"
**Good:** 1 task "Refactor module X" with phases and subphases inside

### Task Structure

```
Task
├── Phase 1: Analysis and Preparation
│   ├── Subphase 1.1: Investigation
│   ├── Subphase 1.2: Action Plan
│   └── Subphase 1.3: Environment Setup
├── Phase 2: Implementation
│   ├── Subphase 2.1: Core
│   ├── Subphase 2.2: Integration
│   └── Subphase 2.3: Tests
└── Phase 3: Verification and Completion
    ├── Subphase 3.1: Code Review
    ├── Subphase 3.2: Documentation
    └── Subphase 3.3: Closure
```

Each phase and subphase has:
- Clear completion criteria
- Status (pending/active/completed)
- Related documents (analysis, plans, results)

### Small tasks — in the harness tracker

Small subtasks (fixing a single bug, refactoring one function, writing a test) are NOT created as separate SLC tasks. They are executed in the harness tracker where the agent works (issue tracker, board, todo list).

SLC task is a container for a large line of work. Inside it — decomposition into phases/subphases, while small steps live in the tracker.

### Connecting documents to project

Only individual relevant documents connect to a project:
- Architectural decisions
- API specifications
- Analysis results
- Key insights

Do not connect: garbage, intermediate notes, duplicating information.

Use `auto_load` for automatic loading of related documents when activating a task.

### Task Lifecycle

1. `create_task` — create with phases in description
2. `activate_task` — make active (context anchor)
3. Work through phases, update status via `update_task`
4. Save intermediate results to the task
5. `/save_context` at the end of each phase
6. `update_task(status: completed)` — on completion
7. `deactivate_task` — MUST deactivate after execution

### Deactivation

After task completion MUST deactivate:
- `deactivate_task` — clears context anchor
- Without deactivation agent continues working in completed task's context
- Before starting new task — activate it: `activate_task`

## Refactoring

### When to refactor
- Code duplication (>2 places with same logic)
- Functions too long (>100 lines)
- Deep nesting (>3 levels)
- Complex conditions (extract method)
- Mixed responsibilities (SRP violation)

### When NOT to refactor
- In the middle of feature implementation (finish first, then clean)
- Without tests (refactoring without tests = blind shooting)
- "While we're at it" — refactoring = separate task with criteria

### How to refactor
1. Ensure tests pass BEFORE starting
2. Small steps: extract → rename → move → inline
3. After each step — tests pass
4. Do not change behavior during refactoring
5. Separate commit (do not mix with features)

## Error Handling Patterns

### Principle: errors are part of API, not side effects
- All errors must be typed (enum, not string)
- Errors must contain context (what triggered, what data)
- User-facing errors — clear messages, not stack trace
- Internal errors — logged with full context

### Rust-specific
- `Result<T, E>` everywhere it can fail
- `?` operator for propagation
- No `unwrap()` / `expect()` in production — only in tests and truly invariant cases
- `anyhow::Result` for applications, `thiserror` for libraries
- Custom error types with `Display` and `From`

### Strategy
- Fail fast on programmer errors (assert, panic)
- Recover gracefully on user/environment errors (Result, retry)
- Log everything at error site, handle at caller

## Observability

### Logging
- Structured logging (serde key-value pairs, not string concatenation)
- Levels: ERROR (action needed), WARN (potential issue), INFO (significant events), DEBUG (debugging)
- Context: request_id, seat_id, document_id
- Do not log secrets and PII

### Metrics (if project uses)
- Latency histograms for key operations
- Counters for events (tool calls, errors, cache hits)
- Gauges for state (active seats, queue depth)

### Tracing
- Span per operation (tool call, LLM request, DB query)
- Correlation ID through entire chain

## API Design Principles

### Consistency
- One naming style (camelCase or snake_case, not mixed)
- One error handling pattern
- One response format (success envelope or direct)

### Minimalism
- Do not expose internal details
- Do not return unnecessary fields
- Do not require unnecessary parameters

### Versioning
- Semver for libraries
- URL versioning for HTTP API (/v1/, /v2/)
- Breaking changes — only in major version

### Backward Compatibility
- New fields — optional with defaults
- Field removal — deprecation warning → removal in major version
- New endpoints — additive, not replacing old ones

## Memory Model

SLC-MCP has a three-tier memory system:

### Episodic Memory (L1→L4)

| Level | Description | TTL |
|-------|-------------|-----|
| L1 | Raw events (diary) | 7 days |
| L2 | Daily summaries | — |
| L3 | Weekly digests | — |
| L4 | Project insights | — |

Progression: L1 → L2 (day summary) → L3 (week digest) → L4 (insights).
Compression happens automatically by timer or manually via `compress_now`.

### Factual Memory (KB)

- Documents in RAG store (all categories except history)
- Semantic search (embeddings) + BM25
- `add_document` — add
- `search` — search
- `consolidate_now` — extract facts from episodic memory to KB (max 10 facts per run, deduplication by cosine >0.92)

### Context Window

- Assembled from: active document + focuses + profiles + core documents
- Limit: `/limit N` or `SLC_CONTEXT_LIMIT_CHARS`
- Compression: block dropping → LLM summarization
- Snapshots: `/save_context <summary>` saves to history

## Focus System

Focuses are current priorities and tasks the agent should keep in view.

### Mind Types
| Type | Purpose |
|------|---------|
| `front` | Current work front (what to do now) |
| `planner` | Planning (strategy, architecture) |
| `executor` | Execution (concrete steps) |
| `critic` | Review and verification (quality, tests) |
| `shared` | Shared focuses (default) |

When reading a specific mind type — returns that mind's focuses + shared.

### Usage
- `focus_add` — add priority at work start
- `focus_update` — update as progress is made
- `focus_remove` — remove completed
- Focuses are part of context (NEVER dropped)

## auto_load vs references

### auto_load
- List of document IDs auto-loaded into context on activation
- Chain: task → project → skills → modules
- Specified when creating task/project
- Works on `activate_document` / `activate_task`

### references
- Regular links between documents
- Not auto-loaded
- Used for navigation and search

## Context Snapshots

On `/save_context <summary>`:
- Creates document in history (category `CONTEXT_SNAPSHOT`)
- Contains: summary + changes + decisions + next_steps
- Preserves context state for future reflection
- Sends SSE event `context_updated`

## Anti-patterns

| Bad | Good |
|-----|------|
| Trying randomly | Analysis → hypothesis → verification |
| "Quick fix" | Decomposition → plan → implementation |
| Ignoring errors | Every error is debugging info |
| Generating without context | Read code → understand → generate |
| Intermediate .md files | Everything in task structure |
| Forgetting focuses | Update focus items as work progresses |
| Not saving context | `/save_context` at phase ends |
| Ignoring compression | React to `compressed: true` |
| Many small tasks | One large task with phases/subphases |
| Forgetting to deactivate | `deactivate_task` after completion |
| Connecting everything to project | Only relevant documents via auto_load |
| Refactoring without tests | Tests first, then refactoring |
| Mixing features and refactoring | Separate commits |
| Irreversible changes without rollback plan | Git stash, backup, feature flags |
