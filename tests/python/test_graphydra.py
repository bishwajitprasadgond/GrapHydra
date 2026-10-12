"""Tests of the Python API (spec section 7)."""

import json
import math
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest
from hypothesis import given, settings
from hypothesis import strategies as st

import graphydra as gv
from graphydra import Graph


@pytest.fixture
def cyber() -> Graph:
    g = Graph.in_memory()
    g.add_node("host-001", ["Host"], {"hostname": "workstation-01", "risk_score": 0.92})
    g.add_node("host-002", ["Host"], {"hostname": "workstation-02", "risk_score": 0.35})
    g.add_node("malware-001", ["Malware"], {"family": "ExampleFamily", "severity": "high"})
    g.add_node("ioc-001", ["Indicator"], {"indicator_type": "domain", "value": "example.invalid"})
    g.add_edge("edge-001", "host-001", "malware-001", "DETECTED", {"confidence": 0.97})
    g.add_edge("edge-002", "malware-001", "ioc-001", "USES_INDICATOR")
    return g


def test_spec_api_example(tmp_path: Path) -> None:
    path = tmp_path / "cybersecurity.graph"
    graph = Graph.open(path, create=True)
    graph.add_node("host-001", labels=["Host"], properties={"hostname": "workstation-01", "risk_score": 0.92})
    graph.add_node("malware-001", labels=["Malware"], properties={"family": "ExampleFamily", "severity": "high"})
    graph.add_edge("edge-001", source="host-001", target="malware-001", edge_type="DETECTED",
                   properties={"confidence": 0.97})
    result = graph.query(
        "MATCH (h:Host) WHERE h.id = $host_id RETURN h.hostname, h.risk_score LIMIT 10",
        parameters={"host_id": "host-001"},
    )
    assert result.columns == ["h.hostname", "h.risk_score"]
    assert result.to_list() == [{"h.hostname": "workstation-01", "h.risk_score": 0.92}]
    graph.close()
    assert graph.closed
    with pytest.raises(gv.ClosedError):
        graph.query("RETURN 1")
    with Graph.open(path) as reopened:
        assert reopened.node_count == 2 and reopened.edge_count == 1
        node = reopened.get_node("host-001")
        assert node is not None and node["hostname"] == "workstation-01" and node.labels == ["Host"]


def test_results_are_typed_and_lazy(cyber: Graph) -> None:
    r = cyber.query("MATCH p = (h:Host)-[e:DETECTED]->(m) RETURN h, e, m.family AS family, p")
    assert len(r) == 1
    row = r[0]
    assert isinstance(row["h"], gv.Node) and row["h"].id == "host-001"
    assert isinstance(row["e"], gv.Edge) and row["e"].type == "DETECTED" and row["e"]["confidence"] == 0.97
    assert isinstance(row["p"], gv.Path) and len(row["p"]) == 1
    assert [n.id for n in row["p"].nodes] == ["host-001", "malware-001"]
    assert r.column("family") == ["ExampleFamily"]
    assert list(r) == [row]
    assert r.rows[0][2] == "ExampleFamily"
    assert cyber.query("MATCH (n) RETURN count(n)").scalar() == 4
    assert json.loads(r.to_json())["rows"][0][0]["kind"] == "node"
    stats = r.stats
    assert stats["rows_returned"] == 1 and stats["edges_traversed"] >= 1


def test_value_round_trip_and_type_safety() -> None:
    g = Graph.in_memory()
    props = {"b": True, "i": -(2**63), "f": 1.5, "s": "héllo", "l": [1, "x", None, [2.0]], "m": {"k": {"z": False}}}
    n = g.add_node("x", properties=props)
    stored = g.get_node("x")
    assert stored is not None
    assert stored.properties == {k: v for k, v in props.items()}
    assert stored == n
    assert type(stored["b"]) is bool and type(stored["i"]) is int
    with pytest.raises(ValueError):
        g.add_node("big", properties={"v": 2**64})
    with pytest.raises(ValueError):
        g.add_node("nan", properties={"v": math.nan})
    with pytest.raises(TypeError):
        g.add_node("obj", properties={"v": object()})
    with pytest.raises(TypeError):
        g.add_node("set", properties={"v": {1, 2}})
    with pytest.raises(gv.ValidationError):
        g.add_node("bad", properties={"id": "reserved"})
    assert g.node_count == 1


def test_error_hierarchy(cyber: Graph) -> None:
    with pytest.raises(gv.QuerySyntaxError) as info:
        cyber.query("MATCH (n)\nWHERE n.x = \nRETURN n")
    assert info.value.line == 3 and info.value.column == 1
    assert isinstance(info.value, gv.GraphError)
    with pytest.raises(gv.QueryError):
        cyber.query("MATCH (n) RETURN missing")
    with pytest.raises(gv.ParameterError):
        cyber.query("MATCH (n) WHERE n.id = $nope RETURN n")
    with pytest.raises(gv.DuplicateIdError):
        cyber.add_node("host-001")
    with pytest.raises(gv.IntegrityError):
        cyber.add_edge("e", "host-001", "missing", "T")
    with pytest.raises(gv.IntegrityError):
        cyber.delete_node("malware-001")
    with pytest.raises(gv.NotFoundError):
        cyber.delete_edge("nope")
    with pytest.raises(gv.ReadOnlyError):
        cyber.query("MATCH (n) DETACH DELETE n", read_only=True)
    with pytest.raises(gv.LimitExceededError):
        cyber.query("MATCH (a)-[*1..1000]->(b) RETURN b")
    with pytest.raises(gv.NotFoundError):
        Graph.open("definitely-missing.graph")
    assert cyber.node_count == 4


def test_transactions(cyber: Graph) -> None:
    with cyber.transaction() as tx:
        tx.add_node("n1")
        tx.query("CREATE (:Tmp {id: 'n2'})")
    assert "n1" in cyber and "n2" in cyber

    with pytest.raises(RuntimeError):
        with cyber.transaction():
            cyber.add_node("n3")
            cyber.set_node_property("host-001", "risk_score", 0.0)
            raise RuntimeError("boom")
    assert "n3" not in cyber
    node = cyber.get_node("host-001")
    assert node is not None and node["risk_score"] == 0.92

    cyber.begin()
    with pytest.raises(gv.TransactionError):
        cyber.begin()
    cyber.delete_node("ioc-001", detach=True)
    assert cyber.in_transaction
    cyber.rollback()
    assert "ioc-001" in cyber
    assert cyber.verify() == []


def test_batch_insert_is_atomic() -> None:
    g = Graph.in_memory()
    with pytest.raises(gv.DuplicateIdError):
        g.add_nodes([{"id": "a"}, gv.Node("b", ["B"]), {"id": "a"}])
    assert g.node_count == 0
    assert g.add_nodes([{"id": f"n{i}", "labels": ["N"], "properties": {"i": i}} for i in range(1000)]) == 1000
    assert g.add_edges(
        [{"id": f"e{i}", "source": f"n{i}", "target": f"n{i + 1}", "type": "NEXT"} for i in range(999)]
    ) == 999
    path = g.shortest_path("n0", "n10", direction="out")
    assert path is not None and len(path) == 10
    assert g.shortest_path("n10", "n0", direction="out") is None


def test_indexes_search_and_explain(cyber: Graph) -> None:
    cyber.create_index("risk_score", kind="sorted")
    cyber.create_index("hostname", kind="text")
    assert {(i["kind"], i["property"]) for i in cyber.indexes()} == {("sorted", "risk_score"), ("text", "hostname")}
    plan = cyber.explain("MATCH (h:Host) WHERE h.risk_score > $r RETURN h", {"r": 0.5})
    assert "IndexRangeScan" in plan.text
    assert [n.id for n, _ in cyber.search_text("workstation 02")][:1] == ["host-002"]
    hits = cyber.find_nodes("Malware", {"severity": "high"})
    assert [n.id for n in hits] == ["malware-001"]
    pairs = cyber.neighbors("malware-001", direction="in")
    assert [(e.id, n.id) for e, n in pairs] == [("edge-001", "host-001")]
    with pytest.raises(gv.IndexDefinitionError):
        cyber.create_index("risk_score", kind="sorted")
    with pytest.raises(ValueError):
        cyber.create_index("x", kind="bogus")  # type: ignore[arg-type]


def test_persistence_recovery_and_locking(tmp_path: Path) -> None:
    path = tmp_path / "p.graph"
    g = Graph.open(path, create=True)
    g.add_node("a", ["A"])
    # Another process cannot open the same file for writing.
    code = (
        "import graphydra, sys\n"
        "try:\n"
        f"    graphydra.Graph.open({str(path)!r})\n"
        "except graphydra.LockedError:\n"
        "    sys.exit(42)\n"
    )
    assert subprocess.run([sys.executable, "-c", code], check=False).returncode == 42
    reader = Graph.open(path, read_only=True)
    assert reader.node_count == 1 and reader.read_only
    with pytest.raises(gv.ReadOnlyError):
        reader.add_node("x")
    # Simulate a crash: the writer process dies without close().
    crash = (
        "import graphydra, os\n"
        f"g = graphydra.Graph.open({str(tmp_path / 'c.graph')!r}, create=True)\n"
        "g.add_node('committed')\n"
        "g.begin(); g.add_node('uncommitted')\n"
        "os._exit(0)\n"
    )
    subprocess.run([sys.executable, "-c", crash], check=True)
    recovered = Graph.open(tmp_path / "c.graph")
    assert "committed" in recovered and "uncommitted" not in recovered
    assert recovered.stats()["recovered_transactions"] == 1
    report = gv.validate(tmp_path / "c.graph")
    assert report["valid"] and report["nodes"] == 1
    g.close()
    reader.close()


def test_import_export(tmp_path: Path, cyber: Graph) -> None:
    cyber.export(tmp_path / "g.json")
    cyber.export(tmp_path / "g.jsonl", format="jsonl")
    cyber.export(tmp_path / "g.graphml", format="graphml")
    cyber.export_csv(tmp_path / "n.csv", tmp_path / "e.csv")
    for importer in ("json", "jsonl", "csv"):
        h = Graph.in_memory()
        if importer == "json":
            h.import_json(tmp_path / "g.json")
        elif importer == "jsonl":
            h.import_jsonl(tmp_path / "g.jsonl")
        else:
            h.import_csv(tmp_path / "n.csv", tmp_path / "e.csv")
        assert sorted(n.id for n in h.nodes()) == sorted(n.id for n in cyber.nodes())
        assert sorted(e.id for e in h.edges()) == sorted(e.id for e in cyber.edges())
    assert "<graphml" in (tmp_path / "g.graphml").read_text(encoding="utf-8")


def test_threads_share_a_graph_and_cancel() -> None:
    g = Graph.in_memory()
    g.add_nodes([{"id": f"n{i}"} for i in range(400)])
    errors: list[BaseException] = []

    def reader() -> None:
        try:
            for _ in range(20):
                assert g.query("MATCH (n) RETURN count(*)").scalar() >= 400
        except BaseException as e:  # noqa: BLE001
            errors.append(e)

    def writer() -> None:
        try:
            for i in range(50):
                g.add_node(f"w{i}")
        except BaseException as e:  # noqa: BLE001
            errors.append(e)

    threads = [threading.Thread(target=reader) for _ in range(4)] + [threading.Thread(target=writer)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert not errors
    assert g.node_count == 450

    token = gv.CancelToken()
    result: list[BaseException] = []

    def slow() -> None:
        try:
            g.query("MATCH (a), (b), (c) RETURN count(*)", cancel=token)
        except gv.CancelledError as e:
            result.append(e)

    t = threading.Thread(target=slow)
    t.start()
    time.sleep(0.2)
    token.cancel()
    t.join(timeout=10)
    assert not t.is_alive() and len(result) == 1
    with pytest.raises(gv.QueryTimeoutError):
        g.query("MATCH (a), (b), (c) RETURN count(*)", timeout=0.05)


@settings(max_examples=150, deadline=None)
@given(
    st.recursive(
        st.none() | st.booleans() | st.integers(-(2**63), 2**63 - 1)
        | st.floats(allow_nan=False, allow_infinity=False) | st.text(max_size=20),
        lambda children: st.lists(children, max_size=4) | st.dictionaries(st.text(min_size=1, max_size=5), children, max_size=4),
        max_leaves=12,
    )
)
def test_property_values_round_trip(value: object) -> None:
    g = Graph.in_memory()
    g.add_node("x", properties={"v": value})
    node = g.get_node("x")
    assert node is not None
    if value is None:
        assert "v" not in node.properties or node["v"] is None
    else:
        assert node["v"] == value
    assert g.query("MATCH (n) RETURN n.v").scalar() == node.get("v")
    assert g.query("RETURN $v AS v", {"v": value}).scalar() == value


def test_tokenize() -> None:
    assert gv.tokenize("Hello, World_2!") == ["hello", "world", "2"]
