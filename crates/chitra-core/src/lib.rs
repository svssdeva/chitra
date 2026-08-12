//! chitra-core — SQLite store + build/incremental pipeline + resolution +
//! bounded impact + review wedge (risk, context, change detection) + structure
//! (communities, flows, architecture) + deterministic export. The library other
//! front-ends (CLI, MCP, Action) are thin shells over.

mod build;
#[cfg(feature = "embeddings")]
mod embed;
mod export;
mod federate;
mod review;
mod store;
mod structure;
mod viz;
mod watch;

pub use build::{build, update, Stats};
#[cfg(feature = "embeddings")]
pub use embed::{hybrid_search, vector_search};
pub use export::export_json;
pub use federate::{federate, FederateStats, RepoRef};
pub use review::{
    changes_markdown, detect_changes, estimate_files, estimate_tokens, minimal_context,
    review_context, risk, risk_v2, top_risks, Detail, Risk, RiskV2,
};
pub use store::{FlowRow, Store};
pub use structure::architecture;
pub use viz::{visualize_html, Mode as VizMode};
pub use watch::{tick, watch_loop, WatchRoot, DEFAULT_INTERVAL_MS};
