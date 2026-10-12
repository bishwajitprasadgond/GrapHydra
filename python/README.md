# GrapHydra

An embedded, local-first graph database for Python. Graphs live in portable
`.graph` files, you query them with a declarative pattern language (GHQL),
and every change is an atomic, crash-safe transaction. There is no server to
run: `pip install` and open a file.

```bash
pip install graphydra
```

Wheels are published for Windows, macOS, and Linux and work with every
CPython from 3.9 on. The engine is written in Rust.

## Quick start

```python
from graphydra import Graph

with Graph.open("library.graph", create=True) as g:
    g.add_node("a-okafor", ["Author"], {"name": "Mira Okafor"})
    g.add_node("b-archive", ["Book"], {"title": "The Clockwork Archive", "year": 2019})
    g.add_node("b-archive-ch1", ["Chapter"], {"title": "The Ledger Room", "number": 1})
    g.add_edge("wrote-1", "a-okafor", "b-archive", "WROTE")
    g.add_edge("has-1", "b-archive", "b-archive-ch1", "HAS_CHAPTER", {"order": 1})

    rows = g.query(
        "MATCH (a:Author)-[:WROTE]->(b:Book)-[:HAS_CHAPTER]->(c) "
        "WHERE a.name = $author RETURN b.title AS book, c.title AS chapter",
        {"author": "Mira Okafor"},
    )
    print(rows.to_list())  # [{'book': 'The Clockwork Archive', 'chapter': 'The Ledger Room'}]
```

## Features

* **Pattern queries:** `MATCH`, `WHERE`, `RETURN`, `ORDER BY`, aggregation,
  bounded multi-hop paths (`-[:NEXT*1..3]->`), `shortestPath`, full-text
  `text_match`, and `CREATE` / `SET` / `REMOVE` / `DELETE`.
* **Transactions:** `with g.transaction():` commits on success and rolls
  back on any exception. A failing statement never leaves partial changes.
* **Crash safety:** commits are flushed to a checksummed journal before they
  return; files are replaced atomically. Recovery runs on open.
* **Indexes:** hash, sorted (ranges and prefixes), and full-text. Use
  `explain()` to see the plan.
* **Safe by default:** `$parameters` instead of string building, query
  timeouts, cancellation, row and visit limits, and `read_only=True` for
  untrusted callers such as AI agents.
* **Concurrency:** one writer per file (OS lock), any number of read-only
  handles; queries release the GIL.
* **Typed:** ships type stubs, and every error is a subclass of `GraphError`.
* **Interop:** import and export JSON, JSON Lines, CSV, and GraphML.

```python
from graphydra import Graph, QueryTimeoutError

with Graph.open("library.graph", read_only=True) as g:
    try:
        result = g.query("MATCH (p:Page) WHERE text_match(p.text, $q) RETURN p", {"q": "brass key"}, timeout=2.0)
    except QueryTimeoutError:
        ...
    print(g.explain("MATCH (b:Book {title: $t}) RETURN b", {"t": "Rivers of Glass"}).text)
```

## Also available

* **GrapHydra Studio:** a desktop app to create databases, draw and explore
  the graph, edit nodes and relationships, and run queries.
* **`graphydra` CLI:** `init`, `validate`, `info`, `query`, `explain`,
  `index`, `import`, `export`, `checkpoint`, and an interactive `shell`.

Both are single executables, available from the
[GitHub releases](https://github.com/bishwajitprasadgond/GrapHydra/releases).

## Documentation

* [Project README](https://github.com/bishwajitprasadgond/GrapHydra#readme)
* [Writing queries](https://github.com/bishwajitprasadgond/GrapHydra/blob/main/examples/writing-queries.md), a step-by-step GHQL guide
* [Library example](https://github.com/bishwajitprasadgond/GrapHydra/blob/main/examples/library/README.md): books, chapters, and pages
* [Query language reference](https://github.com/bishwajitprasadgond/GrapHydra/blob/main/docs/query-language.md)
* [Issues](https://github.com/bishwajitprasadgond/GrapHydra/issues)
