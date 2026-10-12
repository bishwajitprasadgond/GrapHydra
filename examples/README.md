# Examples

| Example | What you learn |
|---|---|
| [Writing queries](writing-queries.md) | GHQL step by step: patterns, filters, parameters, paths, aggregation, full-text search, writes, indexes, and common mistakes |
| [Library: books, chapters, pages](library/README.md) | Modeling ordered, hierarchical content as a graph; 22 example queries with real output; the Python API; opening it in Studio |

Quick start:

```bash
pip install graphydra
python examples/library/build_library.py
python examples/library/run_queries.py
graphydra-studio examples/library/library.graph
```

Every query in these examples is executed by the test suite
(`tests/python/test_examples.py`), so they stay correct as the engine changes.
