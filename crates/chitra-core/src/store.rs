//! SQLite-backed graph store. WAL, prepared statements, recursive-CTE BFS,
//! per-file atomic replace, FTS5 — per the `rusqlite-graph-store` skill.
//! Verified against rusqlite 0.32.1.

use anyhow::Result;
use chitra_lang::{Import, Node, ParsedFile};
use rusqlite::{params, Connection};
use std::collections::HashMap;

pub const SCHEMA_VERSION: i64 = 5;

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS files(
    path     TEXT PRIMARY KEY,
    hash     TEXT NOT NULL,
    language TEXT
);
CREATE TABLE IF NOT EXISTS nodes(
    qualified_name TEXT PRIMARY KEY,
    kind        TEXT,
    name        TEXT,
    file        TEXT,
    line_start  INTEGER,
    line_end    INTEGER,
    language    TEXT,
    signature   TEXT,
    doc         TEXT,
    is_test     INTEGER,
    community_id INTEGER
);
CREATE TABLE IF NOT EXISTS communities(
    id INTEGER PRIMARY KEY, size INTEGER, label TEXT
);
CREATE TABLE IF NOT EXISTS flows(
    id INTEGER PRIMARY KEY, entry TEXT, size INTEGER, criticality REAL
);
CREATE TABLE IF NOT EXISTS flow_memberships(flow_id INTEGER, node TEXT);
CREATE INDEX IF NOT EXISTS idx_flowmem_node ON flow_memberships(node);
CREATE TABLE IF NOT EXISTS imports(
    file   TEXT,
    name   TEXT,
    module TEXT
);
CREATE TABLE IF NOT EXISTS raw_calls(
    file      TEXT,
    caller    TEXT,
    callee    TEXT,
    line      INTEGER,
    qualifier TEXT
);
CREATE TABLE IF NOT EXISTS edges(
    source      TEXT,
    target      TEXT,
    kind        TEXT,
    line        INTEGER,
    confidence  REAL,
    tier        TEXT,
    PRIMARY KEY(source, target, kind, line)
);
CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY, value TEXT);
CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file);
CREATE INDEX IF NOT EXISTS idx_imports_file ON imports(file);
CREATE INDEX IF NOT EXISTS idx_raw_file ON raw_calls(file);
CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target);
CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source);
";

/// Reverse blast-radius: all nodes that transitively reach `target`, bounded by
/// `max_depth`. UNION (not ALL) so cyclic call graphs terminate. Only asserted
/// edges (EXTRACTED/INFERRED) are traversed — AMBIGUOUS ones are surfaced in
/// export/query but never drive impact (honesty: no invented blast radius).
const BFS_SQL: &str = "\
WITH RECURSIVE reach(node, depth) AS (
    SELECT ?1, 0
  UNION
    SELECT e.source, r.depth + 1
    FROM edges e JOIN reach r ON e.target = r.node
    WHERE r.depth < ?2 AND e.tier IN ('EXTRACTED','INFERRED')
)
SELECT DISTINCT node FROM reach WHERE node <> ?1 ORDER BY node";

/// A resolved edge, read back for export.
pub struct EdgeRow {
    pub source: String,
    pub target: String,
    pub kind: String,
    pub line: i64,
    pub confidence: f64,
    pub tier: String,
}

/// Wrap a query as a single FTS5 phrase, so none of its characters are read as
/// operators. Embedded quotes are doubled, which is FTS5's own escape.
fn fts_phrase(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// A detected execution flow (entry point + reachable set).
#[derive(Debug, Clone)]
pub struct FlowRow {
    pub id: i64,
    pub entry: String,
    pub size: i64,
    pub criticality: f64,
}

/// An unresolved call site as stored.
pub struct RawCallRow {
    pub file: String,
    pub caller: String,
    pub callee: String,
    pub line: i64,
    pub qualifier: Option<String>,
}

/// A resolved edge to insert (owned; kind/tier are static labels).
#[derive(Clone)]
pub struct NewEdge {
    pub source: String,
    pub target: String,
    pub kind: &'static str,
    pub line: i64,
    pub confidence: f64,
    pub tier: &'static str,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &str) -> Result<Store> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(SCHEMA)?;
        // Forward-only migrations: v1 predates community_id, v2 predates
        // imports.module. Each ALTER errors harmlessly if the column exists.
        let _ = conn.execute("ALTER TABLE nodes ADD COLUMN community_id INTEGER", []);
        let _ = conn.execute("ALTER TABLE imports ADD COLUMN module TEXT", []);
        let _ = conn.execute("ALTER TABLE raw_calls ADD COLUMN qualifier TEXT", []);
        // v4 predates node docs.
        let _ = conn.execute("ALTER TABLE nodes ADD COLUMN doc TEXT", []);
        conn.execute(
            "INSERT OR REPLACE INTO metadata VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(Store { conn })
    }

    // ---- metadata ----
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO metadata VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .ok())
    }

    // ---- incremental bookkeeping ----

    /// path -> blake3 hash of every file currently in the store.
    pub fn file_hashes(&self) -> Result<HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT path, hash FROM files")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    /// Wipe everything (full build resets before repopulating).
    pub fn clear_all(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM files; DELETE FROM nodes; DELETE FROM imports;
             DELETE FROM raw_calls; DELETE FROM edges;
             DELETE FROM communities; DELETE FROM flows; DELETE FROM flow_memberships;",
        )?;
        Ok(())
    }

    /// Atomically replace one file's rows (nodes/imports/raw_calls + hash).
    /// BEGIN IMMEDIATE takes the write lock up front. Edges are derived globally
    /// afterwards (see `resolve`), so they are not touched here.
    pub fn replace_file(
        &mut self,
        file: &str,
        hash: &str,
        language: &str,
        pf: &ParsedFile,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM nodes WHERE file = ?1", params![file])?;
        tx.execute("DELETE FROM imports WHERE file = ?1", params![file])?;
        tx.execute("DELETE FROM raw_calls WHERE file = ?1", params![file])?;
        tx.execute(
            "INSERT OR REPLACE INTO files VALUES (?1, ?2, ?3)",
            params![file, hash, language],
        )?;
        for n in &pf.nodes {
            tx.execute(
                "INSERT OR REPLACE INTO nodes
                 (qualified_name,kind,name,file,line_start,line_end,language,signature,doc,is_test)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    n.qualified_name,
                    n.kind,
                    n.name,
                    n.file,
                    n.line_start as i64,
                    n.line_end as i64,
                    n.language,
                    n.signature,
                    n.doc,
                    n.is_test as i64
                ],
            )?;
        }
        for imp in &pf.imports {
            tx.execute(
                "INSERT INTO imports VALUES (?1, ?2, ?3)",
                params![file, imp.name, imp.module],
            )?;
        }
        for rc in &pf.raw_calls {
            tx.execute(
                "INSERT INTO raw_calls VALUES (?1,?2,?3,?4,?5)",
                params![
                    file,
                    rc.caller_qualified,
                    rc.callee_name,
                    rc.line as i64,
                    rc.qualifier
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove a deleted file's rows entirely (fail-closed eviction).
    pub fn evict_file(&self, file: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", params![file])?;
        self.conn
            .execute("DELETE FROM nodes WHERE file = ?1", params![file])?;
        self.conn
            .execute("DELETE FROM imports WHERE file = ?1", params![file])?;
        self.conn
            .execute("DELETE FROM raw_calls WHERE file = ?1", params![file])?;
        Ok(())
    }

    // ---- reads for resolution / export ----

    pub fn load_nodes(&self) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT qualified_name,kind,name,file,line_start,line_end,language,signature,coalesce(doc,''),is_test
             FROM nodes ORDER BY qualified_name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Node {
                qualified_name: r.get(0)?,
                kind: r.get(1)?,
                name: r.get(2)?,
                file: r.get(3)?,
                line_start: r.get::<_, i64>(4)? as usize,
                line_end: r.get::<_, i64>(5)? as usize,
                language: r.get(6)?,
                signature: r.get(7)?,
                doc: r.get(8)?,
                is_test: r.get::<_, i64>(9)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every deferred call site, with the path qualifier it was written through.
    pub fn load_raw_calls(&self) -> Result<Vec<RawCallRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT file, caller, callee, line, qualifier FROM raw_calls")?;
        let rows = stmt.query_map([], |r| {
            Ok(RawCallRow {
                file: r.get(0)?,
                caller: r.get(1)?,
                callee: r.get(2)?,
                line: r.get(3)?,
                qualifier: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// file -> imported names (with their module, when the grammar exposed one).
    pub fn load_imports(&self) -> Result<HashMap<String, Vec<Import>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT file, name, module FROM imports")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Import {
                    name: r.get(1)?,
                    module: r.get(2)?,
                },
            ))
        })?;
        let mut m: HashMap<String, Vec<Import>> = HashMap::new();
        for row in rows {
            let (f, imp) = row?;
            m.entry(f).or_default().push(imp);
        }
        Ok(m)
    }

    pub fn all_edges(&self) -> Result<Vec<EdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT source,target,kind,line,confidence,tier FROM edges
             ORDER BY source,target,kind,line",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(EdgeRow {
                source: r.get(0)?,
                target: r.get(1)?,
                kind: r.get(2)?,
                line: r.get(3)?,
                confidence: r.get(4)?,
                tier: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- edges ----

    pub fn clear_edges(&self) -> Result<()> {
        self.conn.execute("DELETE FROM edges", [])?;
        Ok(())
    }

    /// Replace all edges in one transaction. Batching avoids a per-insert fsync
    /// (WAL autocommit) — the difference between a ~15s and a sub-second rebuild
    /// on a 37k-edge graph.
    pub fn replace_edges(&mut self, edges: &[NewEdge]) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM edges", [])?;
        {
            let mut stmt = tx.prepare("INSERT OR IGNORE INTO edges VALUES (?1,?2,?3,?4,?5,?6)")?;
            for e in edges {
                stmt.execute(params![
                    e.source,
                    e.target,
                    e.kind,
                    e.line,
                    e.confidence,
                    e.tier
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn insert_node(&self, n: &Node) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO nodes
             (qualified_name,kind,name,file,line_start,line_end,language,signature,doc,is_test)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                n.qualified_name,
                n.kind,
                n.name,
                n.file,
                n.line_start as i64,
                n.line_end as i64,
                n.language,
                n.signature,
                n.doc,
                n.is_test as i64
            ],
        )?;
        Ok(())
    }

    /// Batch node insert in one transaction — the federation path writes tens of
    /// thousands at once and a per-insert fsync would dominate.
    pub fn insert_nodes(&mut self, nodes: &[Node]) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO nodes
                 (qualified_name,kind,name,file,line_start,line_end,language,signature,doc,is_test)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            )?;
            for n in nodes {
                stmt.execute(params![
                    n.qualified_name,
                    n.kind,
                    n.name,
                    n.file,
                    n.line_start as i64,
                    n.line_end as i64,
                    n.language,
                    n.signature,
                    n.doc,
                    n.is_test as i64
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn insert_edge(
        &self,
        source: &str,
        target: &str,
        kind: &str,
        line: usize,
        confidence: f64,
        tier: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO edges VALUES (?1,?2,?3,?4,?5,?6)",
            params![source, target, kind, line as i64, confidence, tier],
        )?;
        Ok(())
    }

    /// Bounded reverse BFS over asserted edges (who reaches `target` — blast
    /// radius). Sorted → deterministic.
    pub fn impact(&self, target: &str, max_depth: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(BFS_SQL)?;
        let rows = stmt.query_map(params![target, max_depth], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Bounded forward BFS: what `source` transitively reaches (its dependencies).
    pub fn impact_forward(&self, source: &str, max_depth: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE reach(node, depth) AS (
                 SELECT ?1, 0
               UNION
                 SELECT e.target, r.depth + 1
                 FROM edges e JOIN reach r ON e.source = r.node
                 WHERE r.depth < ?2 AND e.kind='CALLS' AND e.tier IN ('EXTRACTED','INFERRED')
             )
             SELECT DISTINCT node FROM reach WHERE node <> ?1 ORDER BY node",
        )?;
        let rows = stmt.query_map(params![source, max_depth], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One node by qualified name.
    pub fn get_node(&self, qn: &str) -> Result<Option<Node>> {
        Ok(self
            .conn
            .query_row(
                "SELECT qualified_name,kind,name,file,line_start,line_end,language,signature,coalesce(doc,''),is_test
                 FROM nodes WHERE qualified_name = ?1",
                params![qn],
                |r| {
                    Ok(Node {
                        qualified_name: r.get(0)?,
                        kind: r.get(1)?,
                        name: r.get(2)?,
                        file: r.get(3)?,
                        line_start: r.get::<_, i64>(4)? as usize,
                        line_end: r.get::<_, i64>(5)? as usize,
                        language: r.get(6)?,
                        signature: r.get(7)?,
                        doc: r.get(8)?,
                        is_test: r.get::<_, i64>(9)? != 0,
                    })
                },
            )
            .ok())
    }

    /// All nodes defined in one file, sorted by line.
    pub fn nodes_in_file(&self, file: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT qualified_name,kind,name,file,line_start,line_end,language,signature,coalesce(doc,''),is_test
             FROM nodes WHERE file = ?1 ORDER BY line_start",
        )?;
        let rows = stmt.query_map(params![file], |r| {
            Ok(Node {
                qualified_name: r.get(0)?,
                kind: r.get(1)?,
                name: r.get(2)?,
                file: r.get(3)?,
                line_start: r.get::<_, i64>(4)? as usize,
                line_end: r.get::<_, i64>(5)? as usize,
                language: r.get(6)?,
                signature: r.get(7)?,
                doc: r.get(8)?,
                is_test: r.get::<_, i64>(9)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- risk aggregates (one pass each; feed the risk-v1 scorer) ----

    /// target -> number of asserted callers (fan-in).
    pub fn fan_in_map(&self) -> Result<HashMap<String, i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT target, count(DISTINCT source) FROM edges
             WHERE kind='CALLS' AND tier IN ('EXTRACTED','INFERRED') GROUP BY target",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    /// Production symbols that have at least one TESTED_BY link.
    pub fn tested_set(&self) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT source FROM edges WHERE kind='TESTED_BY'")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<std::collections::HashSet<_>>>()?)
    }

    /// node -> (ambiguous CALLS edges touching it, total CALLS edges touching it).
    pub fn ambiguous_touch_map(&self) -> Result<HashMap<String, (i64, i64)>> {
        // Count both endpoints of every CALLS edge; AMBIGUOUS flagged separately.
        let mut stmt = self.conn.prepare(
            "SELECT node, sum(amb), count(*) FROM (
                 SELECT source AS node, tier='AMBIGUOUS' AS amb FROM edges WHERE kind='CALLS'
                 UNION ALL
                 SELECT target AS node, tier='AMBIGUOUS' AS amb FROM edges WHERE kind='CALLS'
             ) GROUP BY node",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    // ---- FTS (postprocess, non-fatal at call site) ----

    /// Rebuild the FTS5 index over node identifiers/signatures.
    pub fn rebuild_fts(&self) -> Result<()> {
        self.conn.execute_batch(
            "DROP TABLE IF EXISTS nodes_fts;
             CREATE VIRTUAL TABLE nodes_fts USING fts5(qualified_name, name, file, signature, doc);
             INSERT INTO nodes_fts(qualified_name,name,file,signature,doc)
                 SELECT qualified_name,name,file,signature,coalesce(doc,'') FROM nodes;",
        )?;
        Ok(())
    }

    /// (qualified_name, name, signature) for every node — the corpus hybrid
    /// search vectorizes. Signatures only: bodies never leave the source file.
    #[cfg(feature = "embeddings")]
    pub fn search_docs(&self) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT qualified_name, name, signature FROM nodes ORDER BY qualified_name")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// FTS search: qualified_names of matching nodes, ranked.
    ///
    /// FTS5 has its own query grammar, in which `(`, `"`, `*`, `:` and `-` are
    /// operators. A query typed by a human — or handed over by an assistant —
    /// routinely contains them (`get_config()`, `foo-bar`), and MATCH answers
    /// with a syntax *error* rather than an empty result. Retrying the whole
    /// string as a quoted phrase turns that failure into an ordinary search.
    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<String>> {
        match self.fts_match(query, limit) {
            Ok(hits) => Ok(hits),
            Err(_) => self.fts_match(&fts_phrase(query), limit),
        }
    }

    fn fts_match(&self, query: &str, limit: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT qualified_name FROM nodes_fts WHERE nodes_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- simple graph queries (CLI `query`) ----

    /// Direct callers (source) of a symbol, asserted edges only, sorted.
    pub fn callers_of(&self, sym: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT source FROM edges
             WHERE target = ?1 AND kind='CALLS' AND tier IN ('EXTRACTED','INFERRED')
             ORDER BY source",
        )?;
        let rows = stmt.query_map(params![sym], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Direct callees (target) of a symbol, asserted edges only, sorted.
    pub fn callees_of(&self, sym: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT target FROM edges
             WHERE source = ?1 AND kind='CALLS' AND tier IN ('EXTRACTED','INFERRED')
             ORDER BY target",
        )?;
        let rows = stmt.query_map(params![sym], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Tests linked to a production symbol via TESTED_BY.
    pub fn tests_for(&self, sym: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT target FROM edges WHERE source = ?1 AND kind='TESTED_BY' ORDER BY target",
        )?;
        let rows = stmt.query_map(params![sym], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- structure (postprocess: communities + flows) ----

    /// Persist a community partition: node→id assignments + the community table,
    /// in one transaction.
    pub fn replace_communities(
        &mut self,
        assignments: &[(String, i64)],
        communities: &[(i64, i64, String)], // (id, size, label)
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM communities", [])?;
        tx.execute("UPDATE nodes SET community_id = NULL", [])?;
        {
            let mut set =
                tx.prepare("UPDATE nodes SET community_id = ?2 WHERE qualified_name = ?1")?;
            for (qn, id) in assignments {
                set.execute(params![qn, id])?;
            }
            let mut ins = tx.prepare("INSERT INTO communities VALUES (?1,?2,?3)")?;
            for (id, size, label) in communities {
                ins.execute(params![id, size, label])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// qualified_name -> community id (only assigned nodes).
    pub fn community_map(&self) -> Result<HashMap<String, i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT qualified_name, community_id FROM nodes WHERE community_id IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    /// (id, size, label) per community, largest first.
    pub fn communities(&self) -> Result<Vec<(i64, i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, size, label FROM communities ORDER BY size DESC, id")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn community_members(&self, id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT qualified_name FROM nodes WHERE community_id = ?1 ORDER BY qualified_name",
        )?;
        let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Persist detected flows + their memberships in one transaction.
    pub fn replace_flows(
        &mut self,
        flows: &[FlowRow],
        memberships: &[(i64, String)],
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM flows", [])?;
        tx.execute("DELETE FROM flow_memberships", [])?;
        {
            let mut fi = tx.prepare("INSERT INTO flows VALUES (?1,?2,?3,?4)")?;
            for f in flows {
                fi.execute(params![f.id, f.entry, f.size, f.criticality])?;
            }
            let mut mi = tx.prepare("INSERT INTO flow_memberships VALUES (?1,?2)")?;
            for (fid, node) in memberships {
                mi.execute(params![fid, node])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn flows(&self) -> Result<Vec<FlowRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, entry, size, criticality FROM flows ORDER BY criticality DESC, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(FlowRow {
                id: r.get(0)?,
                entry: r.get(1)?,
                size: r.get(2)?,
                criticality: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn flow_members(&self, id: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT node FROM flow_memberships WHERE flow_id = ?1 ORDER BY node")?;
        let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// node -> max criticality of any flow it belongs to (for risk v2).
    pub fn flow_criticality_map(&self) -> Result<HashMap<String, f64>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.node, max(f.criticality) FROM flow_memberships m
             JOIN flows f ON f.id = m.flow_id GROUP BY m.node",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    /// Indexed source files. Distinct from `node_count` — one file holds many
    /// symbols, so reporting nodes as files overstates a repo's size several
    /// times over.
    pub fn file_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))?)
    }
    pub fn node_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))?)
    }
    pub fn edge_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM edges", [], |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T0.3 acceptance carried forward: bounded impact on a hand-built 20-node
    /// graph. Chain f00->..->f19 plus a shortcut f10->f19 (dedup + branch).
    fn build_20() -> Store {
        let s = Store::open_in_memory().unwrap();
        for i in 0..19 {
            s.insert_edge(
                &format!("f{i:02}"),
                &format!("f{:02}", i + 1),
                "CALLS",
                1,
                1.0,
                "EXTRACTED",
            )
            .unwrap();
        }
        s.insert_edge("f10", "f19", "CALLS", 2, 1.0, "EXTRACTED")
            .unwrap();
        s
    }

    #[test]
    fn bounded_impact_depth_3() {
        let s = build_20();
        let got = s.impact("f19", 3).unwrap();
        assert_eq!(got, vec!["f08", "f09", "f10", "f16", "f17", "f18"]);
    }

    #[test]
    fn full_impact_is_everything_upstream() {
        let s = build_20();
        assert_eq!(s.impact("f19", 100).unwrap().len(), 19);
    }

    /// A pre-v3 database has `imports(file, name)` with no `module`, and a pre-v4
    /// one has `raw_calls` with no `qualifier`. Opening either must add the
    /// columns and keep existing rows readable — the forward-only migration is
    /// the one path an existing user hits on upgrade.
    #[test]
    fn old_database_migrates_forward_without_losing_rows() {
        let path = std::env::temp_dir().join("chitra_migrate_old.db");
        let _ = std::fs::remove_file(&path);
        let db = path.to_string_lossy().to_string();
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE imports(file TEXT, name TEXT);
                 INSERT INTO imports VALUES ('a.py', 'parse');
                 CREATE TABLE raw_calls(file TEXT, caller TEXT, callee TEXT, line INTEGER);
                 INSERT INTO raw_calls VALUES ('a.py', 'a.py::main', 'parse', 3);",
            )
            .unwrap();
        }

        let store = Store::open(&db).unwrap();
        let imports = store.load_imports().unwrap();
        assert_eq!(
            imports.get("a.py").unwrap(),
            &vec![Import {
                name: "parse".to_string(),
                module: None, // pre-v3 rows carry no module evidence
            }]
        );
        let calls = store.load_raw_calls().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].callee, "parse");
        assert_eq!(calls[0].qualifier, None); // pre-v4 rows carry no qualifier
        assert_eq!(
            store.get_meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION.to_string()
        );
    }

    #[test]
    fn ambiguous_edges_excluded_from_impact() {
        let s = Store::open_in_memory().unwrap();
        s.insert_edge("a", "z", "CALLS", 1, 1.0, "EXTRACTED")
            .unwrap();
        s.insert_edge("b", "z", "CALLS", 1, 0.3, "AMBIGUOUS")
            .unwrap();
        // Only the asserted caller `a` reaches z; `b`'s AMBIGUOUS edge is ignored.
        assert_eq!(s.impact("z", 5).unwrap(), vec!["a"]);
    }
}
