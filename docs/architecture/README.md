# Streaming Alternative — Architecture Documentation

This documentation defines the product vision, architecture principles, functional requirements, and engineering workflow for a modern, cross-platform live production and recording application.

## Documents

1. [`ARCHITECTURE_VISION.md`](ARCHITECTURE_VISION.md) — product goals, architectural principles, and target system boundaries.
2. [`FUNCTIONAL_REQUIREMENTS.md`](FUNCTIONAL_REQUIREMENTS.md) — required capabilities and expected behavior.
3. [`ENGINEERING_GUIDELINES.md`](ENGINEERING_GUIDELINES.md) — implementation guidance, testing, observability, and coding-agent workflow.
4. [`ROADMAP.md`](ROADMAP.md) — staged delivery plan and acceptance gates.
5. [`DECISION_LOG_TEMPLATE.md`](DECISION_LOG_TEMPLATE.md) — lightweight architecture decision record template.

## How to use this documentation

Treat these documents as **design constraints and evaluation criteria**, not as an instruction to discard existing work.

Before changing the codebase, the coding agent should:

1. Inspect the current repository, implementation, tests, build system, and documented goals.
2. Map existing components to the concepts described here.
3. Identify gaps, risks, and conflicts with the current implementation.
4. Propose small, testable changes with explicit trade-offs.
5. Preserve working behavior unless a change is justified and validated.

Do not assume that every proposed capability belongs in the first release. Prioritize a reliable media core, explicit data models, observability, and incremental delivery over feature breadth.
