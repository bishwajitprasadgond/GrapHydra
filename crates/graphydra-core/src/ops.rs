//! Logical mutation operations.
//!
//! Every change to a graph is expressed as an [`Op`]. Ops are validated and
//! applied to the store, recorded in the transaction log, and written to the
//! journal on commit, so replaying the journal reproduces committed state.

use serde::{Deserialize, Serialize};

use crate::model::{Edge, IndexDef, Node};
use crate::value::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    AddNode {
        node: Node,
    },
    AddEdge {
        edge: Edge,
    },
    DeleteNode {
        id: String,
    },
    DeleteEdge {
        id: String,
    },
    /// Setting a property to `Null` removes it.
    SetNodeProperty {
        id: String,
        key: String,
        value: Value,
    },
    SetEdgeProperty {
        id: String,
        key: String,
        value: Value,
    },
    AddLabel {
        id: String,
        label: String,
    },
    RemoveLabel {
        id: String,
        label: String,
    },
    CreateIndex {
        index: IndexDef,
    },
    DropIndex {
        index: IndexDef,
    },
    /// Setting a metadata entry to `Null` removes it.
    SetMetadata {
        key: String,
        value: Value,
    },
}
