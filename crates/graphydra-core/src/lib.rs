//! # GrapHydra
//!
//! An embedded, local-first graph database: portable `.graph` files, a
//! declarative graph query language (GHQL), secondary indexes, journaled
//! ACID transactions, and crash recovery, with no server.
//!
//! ```
//! use graphydra_core::{Graph, Node, Edge, QueryOptions};
//!
//! let mut g = Graph::in_memory();
//! g.add_node(Node::new("host-001").with_label("Host").with_property("hostname", "ws-01")).unwrap();
//! g.add_node(Node::new("mal-001").with_label("Malware").with_property("family", "X")).unwrap();
//! g.add_edge(Edge::new("e1", "host-001", "mal-001", "DETECTED")).unwrap();
//!
//! let r = g.query(
//!     "MATCH (h:Host)-[:DETECTED]->(m:Malware) WHERE h.id = $id RETURN m.family",
//!     &QueryOptions::new().param("id", "host-001"),
//! ).unwrap();
//! assert_eq!(r.rows.len(), 1);
//! ```

pub mod config;
pub(crate) mod engine;
pub mod error;
pub mod graph;
pub mod indexes;
pub mod interop;
pub mod language;
pub mod model;
pub mod ops;
pub mod storage;
pub(crate) mod store;
pub mod validator;
pub mod value;

pub use config::{Config, Durability, Limits};
pub use engine::result::{CancellationToken, PlanStep, QueryOptions, QueryPlan, QueryResult, QueryStats, ResultValue};
pub use error::{EntityKind, GraphError, Result, SyntaxError};
pub use graph::{Graph, GraphStats, IndexInfo, OpenOptions};
pub use model::{Edge, IndexDef, IndexKind, IndexTarget, Node, Path};
pub use ops::Op;
pub use store::Direction;
pub use validator::{GraphValidator, ValidationReport, validate_file};
pub use value::{Properties, Value};

/// Library version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
