# Founding project context

This project is intended to become a public, reusable Rust crate for sandbox
egress. It begins as a local Git repository under `~/code`; it should not be
uploaded yet, but its structure, documentation, and development practices
should be suitable for an eventual public GitHub repository.

The crate should capture the common core of Stripe Smokescreen and the related
Rust implementations recorded in [`prior-art.md`](prior-art.md), while using
the `Proxy / Policy / Lease` interface from the original design brief. The
target integration is a Rust sandbox supervisor that already handles some
network setup locally. Comparable environments include microVM services,
Lambda-like systems, and E2B-like systems. This proxy logic is deep enough to
deserve a focused open-source component instead of remaining product-local
networking code.

Smokescreen establishes much of the desired operational character, but this
project is an embeddable Rust module rather than primarily a standalone proxy
server. A small executable around the same library is useful for testing and
for a possible future process boundary; it should not become a separate
implementation.

Development should follow familiar Rust crate practices and be strongly
test-driven. Security invariants, interface behavior, and lifecycle guarantees
must remain visible as the implementation evolves. Keep complexity low,
performance high, and the design simple enough that the finished library feels
both small and dependable.

Research is part of the work. Contributors may clone relevant repositories and
review current web material to understand existing implementations, common
features, and established Rust practices. Record the references and conclusions
that materially shape the design so later contributors do not have to recover
them from chat history.

The repository should have a software-factory feel: standard commands should
run the code, tests, conformance checks, and performance measurements.
Concurrency deserves explicit testing. CPU, memory, thread, and descriptor
behavior should also be measurable as the resource harness develops. The
factory exists to show that the important invariants, interfaces, and behavior
continue to hold, not merely that the attractive path works.

Assume dozens of agents may eventually contribute concurrently. Repository
documentation must preserve the product context through context compaction,
explain the high-level architecture and priorities, and give each contributor
familiar ways to build, test, measure, and understand the project. The goal is
to help future contributors work on the right problems without relying on
private conversational context.

The public README should remain concise while explaining the use case, object
model, integration flow, example usage, security boundary, current scope, and
development commands. More detailed founding requirements, invariants,
architecture, testing strategy, performance evidence, prior art, and roadmap
belong in the linked documents.
