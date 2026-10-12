"""Build the example library graph: authors, books, chapters, and pages.

    python examples/library/build_library.py            # writes examples/library/library.graph
    python examples/library/build_library.py out.graph  # or any path

It also writes ``library.json`` next to this script, so the same data can be
loaded without Python (``graphydra import`` or GrapHydra Studio's
File > Import > JSON).

All books, people, and text are invented for this example.
"""

import sys
from pathlib import Path

from graphydra import Graph

HERE = Path(__file__).resolve().parent

AUTHORS = [
    ("a-okafor", "Mira Okafor", "Nigeria", 1979),
    ("a-varga", "Tomas Varga", "Hungary", 1966),
]

PUBLISHERS = [("pub-lantern", "Lantern House"), ("pub-quarry", "Quarry Press")]
GENRES = [("g-mystery", "Mystery"), ("g-scifi", "Science Fiction"), ("g-nonfiction", "Non-fiction")]
SERIES = [("s-archive", "The Archive Cycle")]

CHARACTERS = [
    ("c-ines", "Ines Duarte", "archivist"),
    ("c-bram", "Bram Holt", "clockmaker"),
    ("c-sela", "Sela Moor", "cartographer"),
]

# Each book: id, title, author, year, isbn, publisher, genres, series (with position), chapters.
# Each chapter: title, then its pages' text. Page numbers run continuously through a book.
BOOKS = [
    {
        "id": "b-archive",
        "title": "The Clockwork Archive",
        "author": "a-okafor",
        "year": 2019,
        "isbn": "978-0-00-000001-1",
        "publisher": "pub-lantern",
        "genres": ["g-mystery", "g-scifi"],
        "series": ("s-archive", 1),
        "chapters": [
            (
                "The Ledger Room",
                [
                    "Ines Duarte unlocked the ledger room an hour before the bells, as she had every morning for nine years.",
                    "The newest ledger was missing. In its place sat a brass key no longer than her thumb.",
                    "She knew one clockmaker in the city who still cut keys by hand: Bram Holt.",
                ],
            ),
            (
                "A Key Without a Lock",
                [
                    "Bram Holt turned the key under his loupe and said it opened nothing he had ever built.",
                    "Its teeth, he insisted, were not teeth at all but a date written in gears.",
                    "Ines copied the date into her notebook: the night the old archive burned.",
                    "Someone wanted the fire remembered, and wanted her to be the one remembering.",
                ],
            ),
            (
                "Ashes in the Catalogue",
                [
                    "The fire report listed every shelf as lost except one, catalogued as empty.",
                    "An empty shelf does not need a lock, Ines thought, unless something was hidden there after.",
                    "Behind the shelf Ines Duarte found a second ledger, written in Bram Holt's careful hand.",
                ],
            ),
        ],
    },
    {
        "id": "b-rivers",
        "title": "Rivers of Glass",
        "author": "a-okafor",
        "year": 2022,
        "isbn": "978-0-00-000002-8",
        "publisher": "pub-lantern",
        "genres": ["g-scifi"],
        "series": ("s-archive", 2),
        "chapters": [
            (
                "Maps That Move",
                [
                    "Sela Moor drew maps of a city whose rivers changed course every winter.",
                    "Her latest map showed a river where the old archive had stood.",
                    "Sela Moor brought it to Ines Duarte, who recognised the brass key etched in the margin.",
                ],
            ),
            (
                "The Glass Current",
                [
                    "Under the ice the river ran clear as glass and carried the ledgers' ashes downstream.",
                    "Ines realised the second ledger was not a record of the fire but a plan for it.",
                    "Bram Holt had not started the fire. He had timed it.",
                ],
            ),
        ],
    },
    {
        "id": "b-machines",
        "title": "A Field Guide to Small Machines",
        "author": "a-varga",
        "year": 2015,
        "isbn": "978-0-00-000003-5",
        "publisher": "pub-quarry",
        "genres": ["g-nonfiction"],
        "series": None,
        "chapters": [
            (
                "Escapements",
                [
                    "An escapement releases a clock's stored energy one tooth at a time.",
                    "The anchor escapement made pendulum clocks accurate enough for navigation.",
                    "Hand-cut escapements show tool marks that identify individual makers.",
                ],
            ),
            (
                "Locks and Keys",
                [
                    "A lever lock opens only when every lever is lifted to exactly the right height.",
                    "Some makers encoded dates or names in the bitting of a key as a signature.",
                    "Such signature keys open nothing; they exist to be recognised.",
                ],
            ),
            (
                "Repair and Restoration",
                [
                    "Restoring a movement begins with a full record of its condition.",
                    "Replace only what is broken, and document every replacement part.",
                ],
            ),
        ],
    },
]

# Pages that cite pages in another book: (citing book, page number, cited book, page number, note).
CITATIONS = [
    ("b-archive", 5, "b-machines", 5, "signature keys"),
    ("b-archive", 4, "b-machines", 1, "gear-encoded dates"),
    ("b-rivers", 3, "b-archive", 2, "the brass key"),
]

CHARACTER_NAMES = {name: cid for cid, name, _ in CHARACTERS}


def build(g: Graph) -> None:
    for aid, name, country, born in AUTHORS:
        g.add_node(aid, ["Author"], {"name": name, "country": country, "born": born})
    for pid, name in PUBLISHERS:
        g.add_node(pid, ["Publisher"], {"name": name})
    for gid, name in GENRES:
        g.add_node(gid, ["Genre"], {"name": name})
    for sid, name in SERIES:
        g.add_node(sid, ["Series"], {"name": name})
    for cid, name, role in CHARACTERS:
        g.add_node(cid, ["Character"], {"name": name, "role": role})

    page_ids: dict[tuple[str, int], str] = {}
    for book in BOOKS:
        bid = book["id"]
        g.add_node(bid, ["Book"], {"title": book["title"], "year": book["year"], "isbn": book["isbn"]})
        g.add_edge(f"{bid}-by", book["author"], bid, "WROTE", {"year": book["year"]})
        g.add_edge(f"{bid}-pub", bid, book["publisher"], "PUBLISHED_BY")
        for genre in book["genres"]:
            g.add_edge(f"{bid}-{genre}", bid, genre, "IN_GENRE")
        if book["series"]:
            sid, position = book["series"]
            g.add_edge(f"{bid}-series", bid, sid, "PART_OF", {"position": position})

        number = 0
        previous_page = previous_chapter = None
        for c_order, (c_title, pages) in enumerate(book["chapters"], start=1):
            cid = f"{bid}-ch{c_order}"
            g.add_node(cid, ["Chapter"], {"title": c_title, "number": c_order, "book": book["title"]})
            g.add_edge(f"{cid}-in", bid, cid, "HAS_CHAPTER", {"order": c_order})
            if previous_chapter:
                g.add_edge(f"{previous_chapter}-next", previous_chapter, cid, "NEXT")
            previous_chapter = cid
            for p_order, text in enumerate(pages, start=1):
                number += 1
                pid = f"{bid}-p{number}"
                page_ids[(bid, number)] = pid
                g.add_node(
                    pid,
                    ["Page"],
                    {"number": number, "text": text, "words": len(text.split()), "book": book["title"]},
                )
                g.add_edge(f"{pid}-in", cid, pid, "HAS_PAGE", {"order": p_order})
                if previous_page:
                    g.add_edge(f"{previous_page}-next", previous_page, pid, "NEXT")
                previous_page = pid
                for name, character in CHARACTER_NAMES.items():
                    if name in text:
                        g.add_edge(f"{pid}-{character}", pid, character, "MENTIONS")
        g.set_node_property(bid, "pages", number)

    # The sequel relationship, and cross-book citations.
    g.add_edge("b-rivers-sequel", "b-rivers", "b-archive", "SEQUEL_OF")
    for i, (src_book, src_page, dst_book, dst_page, note) in enumerate(CITATIONS, start=1):
        g.add_edge(f"cite-{i}", page_ids[(src_book, src_page)], page_ids[(dst_book, dst_page)], "CITES", {"note": note})

    g.set_metadata("description", "Example library: authors, books, chapters, and pages")


def main() -> None:
    target = Path(sys.argv[1]) if len(sys.argv) > 1 else HERE / "library.graph"
    for stale in (target, target.with_name(target.name + ".journal")):
        stale.unlink(missing_ok=True)

    with Graph.open(target, create=True, graph_id="library") as g:
        with g.transaction():
            build(g)
        # Indexes the example queries rely on (see queries.ghql).
        g.create_index("title", kind="sorted")
        g.create_index("name", kind="hash")
        g.create_index("number", kind="sorted")
        g.create_index("text", kind="text")
        g.export(HERE / "library.json", format="json")
        print(f"{target}: {g.node_count} nodes, {g.edge_count} relationships")


if __name__ == "__main__":
    main()
