# SLC-MCP Best Practices

Practical guide for getting the most out of SLC-MCP. These patterns ensure the agent uses all capabilities automatically and in the right places.

## Session Start Protocol

Every session begins with this sequence — do it automatically, don't wait for the user:

```
1. pop_notifications(limit=5)       — check for pending reminders/alerts
2. get_active_task                   — resume where you left off
3. get_active_document               — check context anchor
4. focus_list                        — review current priorities
```

If there's an active task — continue working on it. If not — ask the user what to work on or create a new task.

## Memory Hygiene

### When to `remember`

Record episodic events for things that matter for FUTURE work:

| Record | Skip |
|--------|------|
| Architectural decision made | Routine file reads |
| Bug found and its root cause | Successful compilations |
| Key insight about the codebase | Intermediate debug output |
| User preference expressed | Copy-paste operations |
| Integration point discovered | Formatting changes |
| Performance bottleneck identified | Standard test runs |

### When to `compress_now`

Call when you notice:
- More than 20 L1 records accumulated
- Context feels "heavy" with old diary entries
- Before switching to a completely different topic
- At the end of a long work session

### When to `consolidate_now`

Call when:
- You've been working for a while and insights should be extracted
- Before compressing (consolidation preserves facts, compression loses them)
- When switching projects (extract project-specific knowledge)

### Fact quality

When `consolidate_now` extracts facts, they become permanent KB documents. Good facts:
- "The Rust server uses axum with tower-http middleware"
- "Database migrations live in crates/slc-core/src/storage/"
- "The deployment gives each MCP client an explicit context-token budget"

Bad facts (too vague or ephemeral):
- "There was a bug"
- "The code was changed"
- "Something didn't work"

## Context Management

### The Context Budget

Context is finite. Spend it wisely:

1. **Active document** — always in context, most relevant
2. **Focus items** — always in context, what matters now
3. **Core docs** — always in context, system knowledge
4. **Profiles** — drop when tight, re-add when needed

### When to `update_context`

- After completing a significant subtask
- After making an architectural decision
- Before switching to a different topic
- When the user asks "what's the current state?"
- After `add_document` that changes the picture

### When to `/save_context`

At the end of each work phase:
- Phase complete, moving to next
- Taking a break from the topic
- User asks to "save and continue later"
- Before `compress_now` (snapshot preserves full state)

### Handling `compressed: true`

When context was compressed:
1. **Shorten responses** — less verbose, more concise
2. **Save findings immediately** — `add_document` before they're lost
3. **Suggest `/save_context`** — preserve what's left
4. **Do not ignore the warning** — it means important context may be missing

## Focus System Mastery

### Setting focuses at work start

```
focus_add(title="Complete Phase 2 of auth refactor", priority=8, mind_type="executor")
focus_add(title="Review security implications", priority=6, mind_type="critic")
focus_add(title="Plan migration strategy", priority=5, mind_type="planner")
```

### Updating as work progresses

Found a blocker? Update the focus:
```
focus_update(focus_id="...", description="Blocked on DB schema change, need migration first")
```

Completed something? Remove it:
```
focus_remove(focus_id="...")
```

### Mind type usage

- `front` — what's immediately next (1-2 steps)
- `planner` — strategic decisions, architecture
- `executor` — concrete implementation steps
- `critic` — review, quality, testing
- `shared` — general notes visible everywhere

## Document Patterns

### What to `add_document`

| Category | When | Example |
|----------|------|---------|
| `documentation` | API docs, guides | "REST API specification" |
| `skill` | Reusable procedures | "How to deploy to staging" |
| `custom` | Domain knowledge | "Business rules for invoicing" |
| `code_snippet` | Useful code patterns | "Axum middleware template" |
| `system` | Infrastructure info | "Server configuration" |

### What NOT to add

- Temporary debug output
- One-time analysis results (put in task instead)
- Information that exists in code (search for it instead)
- Duplicates of existing documents

### Using `auto_load`

When creating a task, set up auto_load for related docs:
```
create_task(
    name="Implement payment gateway",
    project_id="proj_payments",
    auto_load=["doc_payment_spec", "doc_stripe_guide", "skill_api_testing"]
)
```

Now when you `activate_task`, all related docs load automatically.

## Search Patterns

### When to `search`

- Before asking the user a question (you might find the answer)
- Before implementing something (similar code might exist)
- When debugging (error patterns in KB)
- When starting work on a topic (what do we already know?)

### Search query tips

- **Specific**: "axum middleware authentication" > "auth"
- **Conceptual**: "how to handle database connections" > "database"
- **Error-driven**: "connection refused postgres" > "error"
- **Pattern-based**: "retry logic with exponential backoff" > "retry"

### After search

- If you find relevant doc → `activate_document` to bring it into context
- If you find nothing → proceed with your knowledge, but consider `add_document` after

## Task Patterns

### Creating well-structured tasks

Good task description:
```
Name: Refactor authentication module
Description:
## Phase 1: Analysis
- Subphase 1.1: Audit current auth flow
- Subphase 1.2: Identify security gaps
- Subphase 1.3: Design new architecture

## Phase 2: Implementation
- Subphase 2.1: Implement JWT service
- Subphase 2.2: Add middleware
- Subphase 2.3: Update all endpoints

## Phase 3: Verification
- Subphase 3.1: Security review
- Subphase 3.2: Integration tests
- Subphase 3.3: Documentation
```

### Progress tracking

As you work through phases:
```
update_task(task_id="...", status="active")  // working on it
// ... do work ...
update_task(task_id="...", description="... updated with results ...")
// ... phase complete ...
/save_context summary="Phase 1 complete: audited auth flow, found3 gaps"
```

### Completion

```
update_task(task_id="...", status="completed")
deactivate_task  // MUST do this
```

## Reminders and Notifications

### Setting reminders

For time-sensitive follow-ups:
```
reminder_create(
    content="Check if CI passed after the merge",
    remind_at="2026-08-15T10:00:00Z"
)
```

### Checking notifications

At the START of every turn — no exceptions:
```
pop_notifications(limit=5)
```

Never skip this. Notifications might contain:
- Reminders you set earlier
- System alerts
- Follow-up tasks

## Profile Management

### Seat profile — workspace settings

Update when you learn about the workspace:
```
update_seat_profile(
    content="Working on Rust project with axum, tokio, sqlx. Tests use cargo nextest. CI is GitLab.",
    timezone="Europe/Moscow"
)
```

### User profile — behavioral preferences

Update when you learn user preferences:
```
update_user_profile(
    content="Prefers concise responses. Likes Rust idioms.不喜欢 unnecessary abstractions. Wants code to be explicit."
)
```

Profiles persist across sessions and shape agent behavior.

## Anti-patterns to Avoid

| Anti-pattern | Why it's bad | What to do instead |
|-------------|-------------|-------------------|
| Never using `remember` | Lose important context between sessions | Record decisions, insights, key findings |
| Never using focuses | Lose track of priorities | Set focuses at start, update as you go |
| Creating many tiny tasks | Clutter, no decomposition | One large task with phases |
| Never compressing | Episodic memory fills up | `compress_now` when20+ L1 records |
| Ignoring `compressed: true` | Miss that context was reduced | React: shorten, save, suggest /save_context |
| Never saving context | Lose work progress | `/save_context` at phase boundaries |
| Searching only when stuck | Waste time on known issues | Search proactively before starting |
| Not deactivating tasks | Agent stuck in old context | Always `deactivate_task` after completion |
| Never updating profiles | Agent doesn't learn preferences | Update when you learn something about user/workspace |
| Ignoring notifications | Miss reminders and alerts | Always `pop_notifications` first |

## Quick Reference

### Session lifecycle
```
START → pop_notifications → get_active_task → focus_list
  ↓
WORK → search → activate_document → do work → update_context → remember
  ↓
PHASE END → /save_context → compress_now → consolidate_now
  ↓
TASK END → update_task(completed) → deactivate_task
  ↓
NEW TASK → create_task → activate_task
```

### Memory lifecycle
```
Event happens → remember(L1)
  ↓
Many L1s → compress_now → L2 (daily summary)
  ↓
7+ L2s → compress_now → L3 (weekly digest)
  ↓
4+ L3s → compress_now → L4 (project insights)
  ↓
Insights → consolidate_now → KB facts (permanent)
```
