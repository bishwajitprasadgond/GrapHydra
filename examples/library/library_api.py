"""Tour of the Python API on the library graph.

    python examples/library/build_library.py
    python examples/library/library_api.py

Works on a temporary copy, so the example graph is never modified.
"""

import shutil
import tempfile
from pathlib import Path

from graphydra import DuplicateIdError, Graph

HERE = Path(__file__).resolve().parent

TABLE_OF_CONTENTS = """
MATCH (b:Book {title: $title})-[h:HAS_CHAPTER]->(c:Chapter)-[:HAS_PAGE]->(p:Page)
RETURN h.order AS chapter, c.title AS title, min(p.number) AS first_page, count(p) AS pages
ORDER BY chapter
"""


def table_of_contents(g: Graph, title: str) -> None:
    print(f"\n{title}")
    # Values go in as parameters, never pasted into the query text.
    for row in g.query(TABLE_OF_CONTENTS, {"title": title}):
        print(f"  {row['chapter']}. {row['title']:<28} page {row['first_page']:>2}  ({row['pages']} pages)")


def read_in_order(g: Graph, book_id: str, pages: int) -> None:
    """Follow NEXT links from a book's first page, like turning pages."""
    first = g.find_nodes("Page", {"book": g.get_node(book_id)["title"], "number": 1}, limit=1)[0]
    page = first
    for _ in range(pages):
        print(f"  p{page['number']}: {page['text']}")
        following = [n for e, n in g.neighbors(page.id, "out", "NEXT")]
        if not following:
            break
        page = following[0]


def main() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "library.graph"
        shutil.copy(HERE / "library.graph", path)
        with Graph.open(path) as g:
            print(f"{g.node_count} nodes, {g.edge_count} relationships")

            for book in g.find_nodes("Book"):
                table_of_contents(g, book["title"])

            print("\nFirst four pages of The Clockwork Archive:")
            read_in_order(g, "b-archive", 4)

            print("\nFull-text search for 'ledger' (best matches first):")
            for node, score in g.search_text("ledger", property="text", limit=3):
                print(f"  {score:.2f}  {node['book']} p{node['number']}")

            path_ = g.shortest_path("a-varga", "c-sela")
            assert path_ is not None
            print("\nTomas Varga to Sela Moor:", " -> ".join(n.id for n in path_.nodes))

            # A transaction is all-or-nothing: the duplicate id below undoes the new chapter too.
            try:
                with g.transaction():
                    g.add_node("b-rivers-ch3", ["Chapter"], {"title": "Thaw", "number": 3, "book": "Rivers of Glass"})
                    g.add_edge("b-rivers-ch3-in", "b-rivers", "b-rivers-ch3", "HAS_CHAPTER", {"order": 3})
                    g.add_node("b-rivers-p1", ["Page"], {"number": 9})  # id already exists
            except DuplicateIdError as e:
                print(f"\nRolled back: {e}")
            print("Chapter 3 exists after rollback:", "b-rivers-ch3" in g)


if __name__ == "__main__":
    main()
