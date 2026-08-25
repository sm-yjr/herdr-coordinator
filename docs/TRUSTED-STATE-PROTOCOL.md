# Trusted State Protocol

## Problem

A coding agent saying “done” is a claim, not proof. Herdr reporting `blocked` is an observation, not necessarily a request for a user decision. A coordinator that merges these signals into one status cannot tell the operator what is known, what is asserted, and what still requires governance.

## Four authorities

1. **Observed authority — Herdr**
   - Source: snapshot and lifecycle/status events.
   - Owns: online/offline, pane identity, `working`, `blocked`, `idle`.
   - Storage: `runtime.json`.
   - Must never overwrite project business status.

2. **Claim authority — commander**
   - Source: `fleet report`.
   - Owns: project progress statement, confidence, declared evidence references.
   - Storage: `claims.json` and current pointer in `fleets.json`.
   - A `done` claim starts in `reported` state.

3. **Verification authority — coordinator process**
   - Source: command actually executed by `fleet verify` in the registered project directory.
   - Owns: argv, cwd, exit code, duration, output tails, pass/fail.
   - A passing command moves the claim to `verified`; a failing latest verification leaves it `reported`.

4. **Governance authority — user/operator**
   - Source: `fleet resolve` and `fleet accept`.
   - Owns: product/design choices and final acceptance.
   - A verified completion moves to `accepted` only after explicit acceptance. `--force` records an intentional override through the acceptance action and note.

## Invariants

```text
observed state  != project state
blocked         != need_decision
reported done   != verified done
verified done   != accepted done
```

Open decisions override the effective project status to `need_decision`, but they do not erase the commander’s last reported status. Resolving the final open decision restores that reported status.

## Claim lifecycle

```text
fleet report done
      │
      ▼
   reported ── fleet verify (exit 0) ──> verified ── fleet accept ──> accepted
      ▲                 │
      └──── verify failure / regression ┘
```

`fleet accept --force` may move an unverified claim directly to accepted. The acceptance timestamp, operator, and supplied note remain in the claim history.

## Attention policy

Attention is a projection, not another source of truth. It is rebuilt from registry, decisions, claims, and runtime. Priority is intentionally governance-first:

```text
formal user decision
runtime loss
long-running unexplained block
commander offline while project active
unverified completion
verified completion awaiting acceptance
status mismatch
```

Consumers should use `fleet attention --json` or `attention.json`; they should not independently infer decisions from raw `blocked` events.

## Controller delivery protocol

Attention remains a projection. Delivery is a separate reliability mechanism that wakes a conversational tower controller without changing the underlying fact authority.

Two sources create deliveries:

- explicit durable events appended by `fleet report`, `ask`, `verify`, `resolve`, and `accept`;
- transitions into runtime-derived attention such as commander offline, unexplained block, runtime loss, and project/runtime mismatch.

Every source has a stable key. `delivery-state.json` stores the append-only inbox offset and the currently active runtime-attention keys, so repeated Plugin reconciliation does not create duplicate work. `deliveries.json` stores delivery state:

```text
pending ── select one idle controller ──> leased ── controller ack ──> acknowledged
   ▲                                      │
   └──── send failure / release / expiry ─┘
```

A lease lasts five minutes. The controller identity is tied to the Herdr agent-session identity when available, so moving a pane does not transfer an active control seat to an unrelated process. File locking prevents two concurrent Plugin hooks from leasing the same event.

The dispatcher prefers `research` for unexplained blocking and state mismatch, `verification` for completion and verification work, and `primary` for governance and general progress. These are attention-routing preferences only; they do not grant a controller authority over project crew.
