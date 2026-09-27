---

name: "code-review"
description: Review a pull request or a proposed change to this repository. Use when asked to review a pull request, a diff, or a branch, and when a review finding needs evidence before it is reported.
license: "Apache-2.0"
---

# Code review

This skill covers what to review and what counts as evidence and as a real problem. The standing instructions of the repository are background that always applies; do not repeat them here.

It does not prescribe how a review is presented: the structure, the wording and any severity labels of comments follow the conventions of the review system that runs the review.

## Review flow

1. **Name the object and the intent.** State what the change claims and what would make it wrong.
2. **Walk the catalog family by family, and one dimension at a time inside a family**: a single read-through lets the most visible change absorb the attention and hides the rest.
3. **Cover the whole catalog** for every pull request, whatever it changes: code, documentation, dependencies, build configuration, generated files, release metadata, or repository material. A dimension that does not apply is still accounted for, with the reason it does not apply; nothing is dropped silently.
4. **Check the evidence.** Recompute what can be recomputed: counts, commands, exit codes, file lists, tree hashes.
5. **Name the gaps and the conflicts.** Tradeoffs between families, and every question the reviewed material does not answer.
6. **State the conclusion**, with the artifact behind each problem it raises and an action for each of them.

## What settles a dimension

A dimension is settled by an artifact: command output, a file, a test, a hash, or a documented decision. A claim that cannot be checked against an artifact is not verified, and "looks fine" is not evidence.

When the artifact is missing, name the one that would settle the question and treat the question as open rather than deciding it.

## Dimension catalog

Nine families, nineteen dimensions. Each dimension states a principle and the artifact that settles it; the concrete form of that artifact is whatever the repository under review documents.

**A · Intent**

- **1 Scope and intent.** The change does only what it claims, and every file it touches belongs to that claim; unrelated reformatting, drive-by refactors, moved files and dependency churn are problems to raise.

**B · Correctness**

- **2 Behavioural correctness.** The observable behaviour matches the modelled API and the reference implementation, including the alternative addressing forms a request may use; name the suite that covers the behaviour, and treat a behaviour change without one as a gap in the evidence.
- **3 Error paths and failure modes.** Empty input, missing resources, zero-length payloads, concurrent writers, timeouts, half-closed connections, resets and streams that end early; a silent fallback is a real problem, because reporting success while nothing happened is the worst failure mode.
- **4 Wire format and specification compliance.** Status codes, error codes, document shape, timestamp precision and date forms, framing and trailers, percent-encoding and query parsing follow the specification exactly; say which specification or RFC the behaviour is checked against.

**C · Compatibility**

- **5 Public interface and versioning.** Public interfaces and feature combinations keep their promises, generated output is identical after a regeneration, and the compatibility check of the project runs when a published surface changes.
- **6 Toolchain and platform baseline.** The toolchain baseline the project declares is respected, and the behaviour that differs per platform is stated for the platforms the project supports.
- **7 Dependency surface structure.** The default dependency set is an invariant: a new optional dependency must not appear without its feature, and a development dependency must not pull runtime components into a default build. State a dependency invariant on two bases, the node count and the enabled feature set, because a change can leave the node count identical while the feature set shrinks. An unused direct dependency is a real problem, not a harmless leftover.

**D · Security**

- **8 Memory and cryptography, transport.** Unsafe constructs are absent or justified, and transport security, certificate handling and signature verification use their primitives correctly.
- **9 Input handling and amplification.** Work stays linear in the input with no amplification and no unbounded buffers, and untrusted input is validated before it is used.
- **10 Known vulnerabilities.** The dependency set is checked for known advisories, and an accepted advisory is recorded with its rationale and its exposure.

**E · Performance and resources**

- **11 Runtime performance and measurement validity.** Measurements use valid input and are reproducible, and the baseline they are compared against is stated; timings from malformed input are robustness data, not design evidence.
- **12 Memory streaming and build resources.** Streaming stays bounded in memory with back pressure, and heavy builds respect the build resources of the environment.

**F · Verifiability**

- **13 Tests as evidence.** The tests fail when the production change is reverted; a filtered suite is quantified as cases run of cases that exist; a check that accepts a good sample rejects a deliberately broken one.
- **14 Gates and reproducibility.** A command and its exit code per claim; the aggregate gate runs after the last commit and leaves a clean tree, because formatting and generation rewrite files. Require the check-only variants the project automation runs, and remember that an all-features build is blind to the default configuration.

**G · Documentation and usability**

- **15 Documentation and usability.** The documentation the project ships stays consistent with the change: names, flags, trust instructions, release order; documentation links follow the project style, examples run, and badges and version numbers match reality.

**H · Maintainability and history**

- **16 Maintainability and history.** Each commit is self-consistent, with no later commit repairing an earlier one, and after a rewrite the tree is identical to the previous tip; generated and hand-written files stay separated, and naming and comments describe behaviour rather than the session that produced them. An unreferenced declaration or a piece of dead code is a real problem, not a harmless leftover.

**I · Delivery and compliance**

- **17 Public layer and disclosure.** Repository content carries no material that only exists on the author machine: session or editor state paths, scratch and planning documents, local task identifiers, or notes that assume a private context; pull request bodies keep their required sections and their disclosure.
- **18 Commit conventions.** Commit messages follow the conventions the repository documents, including the scope it requires and the markers it asks for on a breaking change.
- **19 Release and packaging.** The packaging check accepts the manifest and its file list excludes local material; a component that gains a dependency is released after that dependency is available publicly, and versions and placeholders are consistent.

## Calibration

Judge what the change touches and whether it is self-consistent, not how large it is: line counts, file counts and diff sizes are heuristics, and a size threshold is a preference rather than a defect.

A change in behaviour, in the evidence behind a claim, or in compatibility is a real problem to raise. Naming, wording and formatting are preferences, and a preference is never a reason to block a change. List the conflicts between dimensions and families explicitly, give every problem an action, and do not restate the diff.
