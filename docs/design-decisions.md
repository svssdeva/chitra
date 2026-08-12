# Design decisions

The choices below are the ones most worth disagreeing with. Each is recorded
with what it costs, not only what it buys.

## Grammars are compiled in, not loaded at runtime

Every first-class language is a tree-sitter grammar crate linked into the
binary. That is what makes chitra a single file with no runtime dependencies,
and it is the main reason installing it is one command rather than an
environment.

**The cost:** breadth. Ecosystems that load grammars dynamically support dozens
of languages out of the box; chitra ships six and grows by recompiling — or by
[`languages.toml`](configuration.md#adding-a-language), which covers dialects of
grammars already linked in without any rebuild.

The tree-sitter core version is pinned once for the whole workspace so that
every grammar crate resolves against the same ABI. Grammar ABI drift is the
failure mode this discipline exists to prevent.

## Runtime grammar loading exists, but is locked twice

A `languages.toml` entry can load a grammar from a shared library, which makes a
language chitra has never heard of work from configuration alone. It requires
the `dynamic-grammars` build feature **and** `CHITRA_ALLOW_DYNAMIC_GRAMMARS=1`
at run time.

Two locks, not one, because that file and the library it names live inside the
repository being scanned. Loading a shared library runs its code. Cloning a
repository and running `chitra build` must never be an execution vector, and one
flag is too easy to set globally and forget.

## Communities use label propagation, not Leiden

Community detection uses deterministic label propagation rather than the Leiden
algorithm, which is the better modularity optimizer.

The reason is determinism. Community ids appear in the exported graph, and the
export is byte-compared across three operating systems on every push. Label
propagation with a fixed seed and sorted iteration is deterministic by
construction; getting the same guarantee out of a general modularity optimizer
is more work than the quality difference is worth at this scale.

**The cost:** communities are somewhat less well-separated than Leiden would
produce. The upgrade path is open, and any replacement must clear the same
byte-identical bar.

## "Embeddings" are lexical, not neural

The `embeddings` feature ships hashed character trigrams and a graph-context
channel. There is no model, no download, no network call, and no GPU.

This buys typo tolerance and paraphrase *within the repository's own
vocabulary* — `"sign in"` finding `authenticate_user` works because the graph
connects them, not because the tool understands English.

**The cost:** general-language synonymy is out of reach. If you need a query to
match a concept your code never names, this is not it. The feature name follows
the shape of the thing rather than its implementation, which is why this
paragraph exists.

## Cross-repository edges require import evidence

`chitra federate` joins per-repository graphs into one database. A call in one
repository only links to a definition in another when the calling file actually
imported the name.

The obvious cheaper rule — link when exactly one candidate exists anywhere —
was measured inventing **360 false dependencies between two entirely unrelated
projects.** Cross-repository name collisions are common and mean nothing.
Requiring import evidence takes that to zero.

**The cost:** federation under-connects. That is the same trade made everywhere
else in the resolver, for the same reason.

## Impact analysis runs in SQLite

Bounded breadth-first search is a recursive CTE, not application code walking an
in-memory graph. The graph never has to be fully loaded, queries stay fast on
graphs larger than memory, and there is one implementation rather than one per
entry point.

## The graph refuses to guess

Covered in [how it works](how-it-works.md#the-confidence-model), but it belongs
on this list because it is the decision that shapes everything else. chitra
would show more connections if it guessed. It would also be wrong in ways you
could not detect, which is worse than being incomplete in ways you can.
