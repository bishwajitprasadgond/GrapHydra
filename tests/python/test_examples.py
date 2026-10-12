"""Keep examples/ working: build the library, run every example, check key answers."""

import importlib.util
import json
import re
import sys
from pathlib import Path

import pytest

from graphydra import Graph

EXAMPLES = Path(__file__).resolve().parents[2] / "examples"
LIBRARY = EXAMPLES / "library"
WRITE_CLAUSE = re.compile(r"\b(CREATE|SET|REMOVE|DELETE)\b")


def load(name: str):
    spec = importlib.util.spec_from_file_location(name, LIBRARY / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


@pytest.fixture()
def library(tmp_path: Path) -> Graph:
    build = load("build_library")
    with Graph.open(tmp_path / "library.graph", create=True) as g:
        with g.transaction():
            build.build(g)
        g.create_index("number", kind="sorted")
        g.create_index("text", kind="text")
        yield g


def test_structure(library: Graph) -> None:
    labels = dict(library.stats()["labels"])
    assert labels == {
        "Author": 2,
        "Book": 3,
        "Chapter": 8,
        "Character": 3,
        "Genre": 3,
        "Page": 24,
        "Publisher": 2,
        "Series": 1,
    }
    # Every page belongs to exactly one chapter, every chapter to one book.
    orphans = library.query("MATCH (p:Page) WHERE in_degree(p) = 0 RETURN p").rows
    assert orphans == []
    per_book = library.query(
        "MATCH (b:Book)-[:HAS_CHAPTER]->()-[:HAS_PAGE]->(p:Page) RETURN b.id AS id, count(p) AS n ORDER BY id"
    )
    assert [(r["id"], r["n"]) for r in per_book] == [("b-archive", 10), ("b-machines", 8), ("b-rivers", 6)]


def test_every_read_example_runs_read_only(library: Graph) -> None:
    run = load("run_queries")
    examples = run.examples(LIBRARY / "queries.ghql")
    assert len(examples) == 14
    for title, statement in examples:
        result = library.query(statement, read_only=True)
        assert len(result) > 0, f"{title} returned no rows"


def test_key_answers(library: Graph) -> None:
    run = load("run_queries")
    answers = {title.split(".")[0]: library.query(q, read_only=True) for title, q in run.examples(LIBRARY / "queries.ghql")}
    assert answers["6"].rows == [(2, "A Key Without a Lock")]
    assert answers["7"].column("page") == [4, 5, 6]
    found = list(zip(answers["8"].column("book"), answers["8"].column("page")))
    assert found == [("Rivers of Glass", 3), ("The Clockwork Archive", 2)]
    assert answers["14"].column("hops") == [5]


def test_write_examples_apply_in_order(library: Graph) -> None:
    run = load("run_queries")
    results = [library.query(q) for _, q in run.examples(LIBRARY / "writes.ghql")]
    assert results[-1].rows == [(4, 11)]  # chapter 4 added; one of its two pages deleted again
    assert library.verify() == []


def test_committed_json_matches_the_builder(library: Graph, tmp_path: Path) -> None:
    """library.json (for CLI/Studio users) must be regenerated whenever the builder changes."""
    committed = json.loads((LIBRARY / "library.json").read_text(encoding="utf-8"))
    out = tmp_path / "fresh.json"
    library.export(out)
    fresh = json.loads(out.read_text(encoding="utf-8"))
    key = lambda items: sorted(items, key=lambda x: x["id"])  # noqa: E731
    assert key(fresh["nodes"]) == key(committed["nodes"])
    assert key(fresh["edges"]) == key(committed["edges"])


def test_results_md_is_current(tmp_path: Path) -> None:
    """RESULTS.md is generated; fail if the examples changed without regenerating it."""
    build = load("build_library")
    run = load("run_queries")
    path = tmp_path / "library.graph"
    with Graph.open(path, create=True, graph_id="library") as g:
        with g.transaction():
            build.build(g)
        for prop, kind in [("title", "sorted"), ("name", "hash"), ("number", "sorted"), ("text", "text")]:
            g.create_index(prop, kind=kind)
    expected = (LIBRARY / "RESULTS.md").read_text(encoding="utf-8")
    assert run.results(path) == expected, "run: python examples/library/run_queries.py --results"


def guide_blocks() -> list[str]:
    text = (EXAMPLES / "writing-queries.md").read_text(encoding="utf-8")
    return [block.strip() for block in re.findall(r"```cypher\n(.*?)```", text, flags=re.S)]


def test_every_query_in_the_writing_guide_runs(library: Graph) -> None:
    blocks = guide_blocks()
    assert len(blocks) >= 15  # guards against the extraction regex silently matching nothing
    params = {"title": "The Clockwork Archive"}
    for query in blocks:
        used = {k: v for k, v in params.items() if f"${k}" in query}
        if " INDEX ON " in query:  # schema change: run it, then undo it
            library.query(query)
            library.drop_index("words", kind="sorted")
        elif WRITE_CLAUSE.search(query) and not query.startswith("EXPLAIN"):
            library.begin()  # each write must work, starting from the original data
            try:
                library.query(query, used)
            finally:
                library.rollback()
        else:
            result = library.query(query, used, read_only=True)
            assert query.startswith("EXPLAIN") or len(result) > 0, f"no rows:\n{query}"
    assert library.node_count == 46
