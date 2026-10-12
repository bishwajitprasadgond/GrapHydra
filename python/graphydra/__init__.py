"""GrapHydra: an embedded, local-first graph database.

Quick start::

    from graphydra import Graph

    with Graph.open("cyber.graph", create=True) as g:
        g.add_node("host-001", labels=["Host"], properties={"hostname": "ws-01"})
        g.add_node("mal-001", labels=["Malware"], properties={"family": "X"})
        g.add_edge("e1", "host-001", "mal-001", "DETECTED", {"confidence": 0.97})
        for row in g.query(
            "MATCH (h:Host)-[:DETECTED]->(m) WHERE h.id = $id RETURN m.family",
            {"id": "host-001"},
        ):
            print(row["m.family"])

The engine is implemented in Rust; this package is a thin typed wrapper.
"""

from graphydra._graphydra import (
    FORMAT_VERSION,
    CancelledError,
    CancelToken,
    ClosedError,
    DuplicateIdError,
    Edge,
    FormatError,
    Graph,
    GraphError,
    IndexDefinitionError,
    IntegrityError,
    LimitExceededError,
    LockedError,
    Node,
    NotFoundError,
    ParameterError,
    Path,
    QueryError,
    QueryPlan,
    QueryResult,
    QuerySyntaxError,
    QueryTimeoutError,
    ReadOnlyError,
    StorageError,
    Transaction,
    TransactionError,
    ValidationError,
    __version__,
    tokenize,
    validate,
)

__all__ = [
    "FORMAT_VERSION",
    "CancelToken",
    "CancelledError",
    "ClosedError",
    "DuplicateIdError",
    "Edge",
    "FormatError",
    "Graph",
    "GraphError",
    "IndexDefinitionError",
    "IntegrityError",
    "LimitExceededError",
    "LockedError",
    "Node",
    "NotFoundError",
    "ParameterError",
    "Path",
    "QueryError",
    "QueryPlan",
    "QueryResult",
    "QuerySyntaxError",
    "QueryTimeoutError",
    "ReadOnlyError",
    "StorageError",
    "Transaction",
    "TransactionError",
    "ValidationError",
    "__version__",
    "tokenize",
    "validate",
]
