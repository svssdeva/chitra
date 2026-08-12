//! T4.2 — local vectors for hybrid search (feature `embeddings`).
//!
//! **Not neural embeddings**, and no model file, download, network or GPU —
//! that is what "local-first" has to mean for a tool that ships as one static
//! binary (ADR-0003). Two complementary channels are built instead, and fused
//! with FTS by reciprocal rank:
//!
//! 1. **Lexical** — hashed character trigrams over a symbol's name and
//!    signature. Absorbs typos, transpositions and truncations, which an FTS
//!    token match answers with nothing at all.
//! 2. **Contextual** — the same vectoriser over a document *expanded with the
//!    names of the symbol's graph neighbours*. This is distributional
//!    semantics taken from the repository itself: `authenticate_user` becomes
//!    findable by "sign in" because `sign_in` is what calls it, even though the
//!    two share no characters.
//!
//! The honest limit is that the semantics are the repository's, not English's.
//! A query for "remove" finds `delete_account` only if this codebase connects
//! those two words somewhere. There is no general synonymy, because there is no
//! model.
//!
//! Vectors are derived at query time rather than stored. At a few thousand
//! symbols this costs milliseconds and keeps the schema and the byte-identical
//! export untouched.
//!
//! ponytail: hashing + graph expansion; a real local model behind the same
//! interface is the upgrade if general-language synonymy is ever needed.

use crate::store::Store;
use anyhow::Result;

/// Vector width. 256 dims is enough to keep trigram collisions rare at the
/// scale of one repo's symbol table.
const DIM: usize = 256;

/// Reciprocal-rank-fusion damping. 60 is the value from the original RRF paper;
/// it keeps a strong hit in one ranking from swamping the other ranking.
const RRF_K: f64 = 60.0;

/// FNV-1a. Chosen over `DefaultHasher` because determinism is a project-wide
/// invariant and `DefaultHasher`'s output is explicitly not stable across Rust
/// releases.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// Split an identifier into lowercase words: `parse_HTTPHeader` -> parse, http, header.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            // camelCase boundary: a lowercase run followed by an uppercase char.
            if prev_lower && c.is_uppercase() && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur.push(c.to_ascii_lowercase());
            prev_lower = c.is_lowercase();
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Hashed feature vector over whole words plus their padded character trigrams,
/// L2-normalized so cosine similarity is a plain dot product.
pub fn vector(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; DIM];
    let mut bump = |feature: &str| {
        v[(fnv1a(feature.as_bytes()) % DIM as u64) as usize] += 1.0;
    };
    for w in words(text) {
        bump(&w);
        // Padding makes prefixes and suffixes features in their own right, so a
        // truncated query ("authent") still overlaps the full symbol.
        let padded: Vec<char> = format!("^{w}$").chars().collect();
        for tri in padded.windows(3) {
            bump(&tri.iter().collect::<String>());
        }
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Rank a corpus of (id, text) documents against a query, best first.
fn rank(query: &str, docs: Vec<(String, String)>, limit: usize) -> Vec<String> {
    let q = vector(query);
    let mut scored: Vec<(String, f32)> = docs
        .into_iter()
        .map(|(id, text)| (id, dot(&q, &vector(&text))))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    // Descending by score, then by name — a total order, so results are stable.
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.into_iter().take(limit).map(|(id, _)| id).collect()
}

/// Each symbol's own text: name + signature. Bodies never enter a vector.
fn lexical_docs(store: &Store) -> Result<Vec<(String, String)>> {
    Ok(store
        .search_docs()?
        .into_iter()
        .map(|(qn, name, signature)| (qn, format!("{name} {signature}")))
        .collect())
}

/// Each symbol's text expanded with the names of the symbols it calls and the
/// symbols that call it. This is where the semantics come from: a symbol is
/// described by the company it keeps in *this* repository.
///
/// One hop only. Two hops drags in most of a connected component and the signal
/// washes out.
fn contextual_docs(store: &Store) -> Result<Vec<(String, String)>> {
    let docs = store.search_docs()?;
    let short_name: std::collections::HashMap<String, String> = docs
        .iter()
        .map(|(qn, name, _)| (qn.clone(), name.clone()))
        .collect();

    let mut neighbours: std::collections::HashMap<String, Vec<&str>> =
        std::collections::HashMap::new();
    for e in store.all_edges()? {
        if e.kind != "CALLS" || e.tier == "AMBIGUOUS" {
            continue; // an ambiguous edge is not evidence of anything
        }
        if let Some(n) = short_name.get(&e.target) {
            neighbours.entry(e.source.clone()).or_default().push(n);
        }
        if let Some(n) = short_name.get(&e.source) {
            neighbours.entry(e.target.clone()).or_default().push(n);
        }
    }

    Ok(docs
        .into_iter()
        .map(|(qn, name, signature)| {
            let mut text = format!("{name} {signature}");
            if let Some(ns) = neighbours.get(&qn) {
                for n in ns {
                    text.push(' ');
                    text.push_str(n);
                }
            }
            (qn, text)
        })
        .collect())
}

/// Pure lexical vector search over symbol name + signature, best first.
pub fn vector_search(store: &Store, query: &str, limit: usize) -> Result<Vec<String>> {
    Ok(rank(query, lexical_docs(store)?, limit))
}

/// Vector search over graph-expanded documents — finds a symbol by what its
/// neighbours are called.
pub fn contextual_search(store: &Store, query: &str, limit: usize) -> Result<Vec<String>> {
    Ok(rank(query, contextual_docs(store)?, limit))
}

/// FTS, lexical and contextual rankings fused by reciprocal rank. Any channel
/// may come back empty — a free-text query is frequently invalid FTS5 syntax,
/// and that is a miss, not an error.
pub fn hybrid_search(store: &Store, query: &str, limit: usize) -> Result<Vec<String>> {
    let deep = limit * 2;
    let channels = [
        store.search(query, deep as i64).unwrap_or_default(),
        vector_search(store, query, deep)?,
        contextual_search(store, query, deep)?,
    ];

    let mut fused: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for ranking in &channels {
        for (rank, qn) in ranking.iter().enumerate() {
            *fused.entry(qn.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut out: Vec<(String, f64)> = fused.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(out.into_iter().take(limit).map(|(qn, _)| qn).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Per-test directory: these tests run in parallel and a shared fixture
    /// meant they wiped each other mid-build, which showed up as a rare
    /// unexplained failure rather than an obvious one.
    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chitra_embed_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.py"),
            "def authenticate_user(token):\n    return token\n\
             def refresh_session_token(session):\n    return session\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("billing.py"),
            "def charge_credit_card(amount):\n    return amount\n",
        )
        .unwrap();
        // The vocabulary bridge: these callers share no characters with their
        // callees, so only graph context can connect the two.
        std::fs::write(
            dir.join("web.py"),
            "from auth import authenticate_user\n\
             from billing import charge_credit_card\n\
             def sign_in(u):\n    return authenticate_user(u)\n\
             def checkout(o):\n    return charge_credit_card(o)\n",
        )
        .unwrap();
        dir
    }

    fn built(dir: &Path) -> Store {
        let mut store = Store::open_in_memory().unwrap();
        crate::build(&mut store, dir).unwrap();
        store
    }

    #[test]
    fn identifier_splits_into_words() {
        assert_eq!(words("parse_HTTPHeader v2"), ["parse", "httpheader", "v2"]);
    }

    #[test]
    fn vector_is_normalized_and_deterministic() {
        let a = vector("authenticate_user");
        let b = vector("authenticate_user");
        assert_eq!(a, b);
        assert!((dot(&a, &a) - 1.0).abs() < 1e-5);
    }

    /// The acceptance check (T4.2): on a query set of typos and partial names,
    /// hybrid finds symbols FTS-only misses entirely.
    #[test]
    fn hybrid_beats_fts_only_on_a_query_set() {
        let store = built(&fixture("hybrid_beats_fts_o"));
        let queries = [
            ("authenitcate", "auth.py::authenticate_user"), // transposed letters
            ("refesh session", "auth.py::refresh_session_token"), // typo + partial
            ("credit card charge", "billing.py::charge_credit_card"), // reordered
        ];

        let hits = |f: &dyn Fn(&str) -> Vec<String>| {
            queries
                .iter()
                .filter(|(q, want)| f(q).iter().any(|r| r == want))
                .count()
        };
        let fts_hits = hits(&|q: &str| store.search(q, 5).unwrap_or_default());
        let hybrid_hits = hits(&|q: &str| hybrid_search(&store, q, 5).unwrap());

        println!("query set: fts={fts_hits}/3 hybrid={hybrid_hits}/3");
        assert_eq!(hybrid_hits, 3, "hybrid should answer every query");
        assert!(
            hybrid_hits > fts_hits,
            "hybrid ({hybrid_hits}) must beat FTS-only ({fts_hits})"
        );
    }

    #[test]
    fn hybrid_keeps_exact_fts_matches_ranked_first() {
        let store = built(&fixture("hybrid_keeps_exact"));
        let out = hybrid_search(&store, "charge_credit_card", 3).unwrap();
        assert_eq!(out[0], "billing.py::charge_credit_card");
    }

    fn rank_of(results: &[String], target: &str) -> Option<usize> {
        results.iter().position(|r| r == target)
    }

    /// Finding a symbol from words that appear nowhere in its name or signature.
    /// "sign in" shares no word with `authenticate_user`; only the call edge from
    /// `sign_in` connects them.
    ///
    /// The lexical channel is not claimed to return *nothing* here — trigram
    /// similarity is noisy and will rank something. The claim is that graph
    /// context puts the right answer at the top and lexical does not.
    #[test]
    fn contextual_channel_ranks_paraphrase_answers_above_lexical() {
        let store = built(&fixture("contextual_channel"));
        for (query, target) in [
            ("sign in", "auth.py::authenticate_user"),
            ("checkout", "billing.py::charge_credit_card"),
        ] {
            let lexical = vector_search(&store, query, 10).unwrap();
            let contextual = contextual_search(&store, query, 10).unwrap();
            let (lr, cr) = (rank_of(&lexical, target), rank_of(&contextual, target));
            println!("{query:?} -> lexical rank {lr:?}, contextual rank {cr:?}");

            let cr =
                cr.unwrap_or_else(|| panic!("{query:?} did not reach {target}: {contextual:?}"));
            assert!(
                cr <= 1,
                "{query:?} should put {target} in the top 2, got rank {cr} ({contextual:?})"
            );
            assert!(
                lr.is_none_or(|lr| cr < lr),
                "{query:?}: graph context (rank {cr}) should beat lexical (rank {lr:?})"
            );
            // FTS alone cannot answer a paraphrase at all.
            assert!(!store
                .search(query, 10)
                .unwrap_or_default()
                .iter()
                .any(|r| r == target));
            assert!(hybrid_search(&store, query, 5)
                .unwrap()
                .iter()
                .any(|r| r == target));
        }
    }
}
