# GHQL: the GrapHydra Query Language

GHQL is a declarative, Cypher-style pattern language. You describe *what* to
match; the planner decides how (which index, which starting node, which
direction to expand).

Keywords are case-insensitive. Labels, edge types and property names are
case-sensitive. Use `` `backticks` `` for names with spaces or reserved words.
Comments: `// line` and `/* block */`.

## Statements

```
[EXPLAIN | PROFILE] statement

statement :=
    { MATCH patterns [WHERE expr] }      -- zero or more; merged into one match
    { update }                           -- CREATE / SET / REMOVE / [DETACH] DELETE
    [ RETURN [DISTINCT] (* | item [AS name], ...)
        [ORDER BY expr [ASC|DESC], ...] [SKIP n] [LIMIT n] ]
  | CREATE [HASH|SORTED|TEXT] INDEX ON (NODES|EDGES)(property)
  | DROP   [HASH|SORTED|TEXT] INDEX ON (NODES|EDGES)(property)
  | SHOW INDEXES
```

A statement must end in `RETURN` or contain an update clause. A script is a
`;`-separated list of statements (`execute_script`, `graphydra query -f`).

* `EXPLAIN` returns the plan without executing anything (even for mutations).
* `PROFILE` executes and attaches the plan plus statistics.

## Patterns

```
pattern  := [pathVar =] [shortestPath(] node (rel node)* [)]
node     := ( [var] {:Label} [{key: expr, ...}] )
rel      := -[ [var] [:TYPE{|TYPE}] [*[min][..[max]]] [{key: expr, ...}] ]->
          | <-[ ... ]-          (incoming)
          | -[ ... ]-           (either direction)
          | -->  <--  --        (anonymous shorthands)
```

* `(n:A:B)`: the node must have **all** listed labels.
* `[:T|U]`: either type.
* `{key: value}` on a node or edge is an equality filter.
* `n.id` is the node id (ids are not properties; `id` is a reserved key).
* Multiple comma-separated patterns form a join on shared variables, or a
  cartesian product when they share none.
* An edge is bound at most once per result row (relationship uniqueness).

### Variable-length traversal

| Syntax | Meaning |
|---|---|
| `*2` | exactly 2 hops |
| `*1..3` | 1 to 3 hops |
| `*..4` | 1 to 4 hops |
| `*0..2` | includes the start node itself (0 hops) |
| `*`, `*2..` | **rejected**: unbounded traversal must give a maximum |

* The maximum is capped by `Limits::max_traversal_depth` (default 64).
* Paths never reuse an edge (trail semantics), which also guarantees
  termination on cycles. Nodes may repeat.
* Each distinct path is one result row; use `DISTINCT` for unique endpoints.
* The variable (`[r*1..3]`) is a list of edges.
* Property maps on variable-length relationships must be constants and apply
  to every edge on the path.

### Shortest path

```cypher
MATCH p = shortestPath((a {id: $from})-[:ROAD*..10]-(b {id: $to})) RETURN p, length(p)
```

* Unweighted breadth-first search with a minimum length of 0 or 1.
* Returns one shortest path per matched endpoint pair.
* If the target is selective (≤ 16 candidates by id or index), it is bound
  first so the search stops early.

## Updates

```cypher
CREATE NODE (:Host {id: 'h1', name: 'ws'})          -- NODE / EDGE keywords optional
CREATE (a:P {id: 'a'})-[:KNOWS {since: 2020}]->(b:P {id: 'b'}) RETURN a, b
MATCH (a {id:'a'}), (b {id:'b'}) CREATE (a)-[:PEER]-(b)       -- '-' creates an undirected edge
MATCH (n:Host) SET n.risk = 0.7, n:Reviewed, n += {seen: true}
MATCH (n {id:'a'}) SET n = {only: 1}                -- replace all properties
MATCH (n {id:'a'}) REMOVE n.tmp, n:Reviewed
MATCH ()-[e:OLD]->() DELETE e
MATCH (n:Temp) DETACH DELETE n                      -- deletes incident edges too
```

* **Snapshot semantics:** all rows are matched first, then updates apply in
  clause order, row by row.
* **Atomicity:** if any part fails, every change of the statement is undone.
* **Node deletion policy:** `DELETE n` fails with `NodeHasEdges` while edges
  remain; use `DETACH DELETE`.
* **Ids:** `{id: ...}` in `CREATE` sets the id; otherwise one is generated
  (`n-<hex>` / `e-<hex>`). Ids are immutable.
* `SET n.k = null` removes a property.
* Node and edge values cannot be stored as properties.

## Expressions

| Category | Operators / forms |
|---|---|
| Boolean | `AND`, `OR`, `XOR`, `NOT` (three-valued: `true`/`false`/`null`) |
| Comparison | `=`, `<>` / `!=`, `<`, `<=`, `>`, `>=` |
| Null tests | `IS NULL`, `IS NOT NULL` |
| Membership | `x IN [list]` |
| Strings | `CONTAINS`, `STARTS WITH`, `ENDS WITH` (case-sensitive, code-point based) |
| Arithmetic | `+ - * / % ^` (`int / int` truncates; overflow and division by zero are errors) |
| Concatenation | `'a' + 1`, `[1] + [2]`, `list + elem` |
| Access | `n.prop`, `map.key`, `list[i]` (negative indexes count from the end), `map['k']` |
| Literals | `1`, `-2.5e3`, `'str'`, `"str"`, `true`, `null`, `[1, 2]`, `{k: v}` |
| Parameters | `$name` (always prefer parameters to string building) |
| Conditional | `CASE WHEN c THEN x ELSE y END`, `CASE v WHEN 1 THEN ... END` |

**Null semantics.** Any comparison with `null` is `null`. `WHERE` keeps only
rows that evaluate to `true`.

**Comparing types.**
* Numbers compare across int and float (`1 = 1.0`).
* Values of different kinds are unequal, and comparing them with `<` gives `null`.

**ORDER BY total order.** Ascending order is booleans, numbers, strings,
lists, maps, nodes, edges, paths, then `null` last. `DESC` reverses it, so
nulls come first.

## Functions

| Group | Functions |
|---|---|
| Entities | `id(x)`, `labels(n)`, `type(e)`, `keys(x)`, `properties(x)`, `startNode(e)`, `endNode(e)`, `degree(n)`, `in_degree(n)`, `out_degree(n)` |
| Paths | `length(p)`, `nodes(p)`, `relationships(p)` / `edges(p)` |
| Strings | `toLower`, `toUpper`, `trim`, `ltrim`, `rtrim`, `replace`, `substring(s, start[, len])`, `left`, `right`, `split`, `reverse`, `size` |
| Conversion | `toString`, `toInteger`, `toFloat`, `toBoolean`, `coalesce(a, b, ...)` |
| Math | `abs`, `ceil`, `floor`, `round`, `sqrt`, `sign`, `exp`, `log`, `log10` |
| Lists | `size`, `head`, `last`, `tail`, `range(start, end[, step])`, `reverse` |
| Predicates | `exists(x)`, `text_match(value, 'terms')` |
| Aggregates | `count(*)`, `count([DISTINCT] x)`, `sum`, `avg`, `min`, `max`, `collect` |

**Aggregation:** non-aggregated `RETURN` items are the grouping keys.
* An aggregate over zero rows still returns one row (`count` = 0, `sum` = 0,
  `collect` = `[]`, `avg`/`min`/`max` = null) when there are no grouping keys.
* `ORDER BY` in an aggregating query may use returned columns and aggregates.

**Full-text:** `text_match(v, q)` is true when every term of `q` appears in
`v` (a string or a list of strings).
* A term is a Unicode-lowercased run of letters and digits; everything else
  separates terms. The text index uses the same tokenizer, so indexed and
  unindexed results are identical.
* For ranked results use `Graph.search_text()`.

## Indexes and planning

Always maintained: the id index, the label index, the edge-type index, and
adjacency lists grouped by edge type.

| Index | Serves |
|---|---|
| `HASH` | `=`, `IN` |
| `SORTED` | `=`, `IN`, `<` `<=` `>` `>=` ranges (both bounds combined), `STARTS WITH` |
| `TEXT` | `text_match`, `search_text` |

Indexes are global per property: one index covers all nodes, or all edges,
that have the property.

The planner works in four steps:
1. Split `WHERE` into its `AND` conjuncts.
2. For every pattern element, compute the exact candidate count of each
   possible access path: id lookup, index seek / range / prefix / text,
   label scan, edge-type scan, or full scan.
3. Start from the smallest, then expand along adjacency lists.
4. Run each filter as soon as its variables are bound.

Constant sub-expressions such as `$p + 0.1` are folded first, so they can
use indexes. `EXPLAIN` lists every candidate it considered and suggests a
missing index when a scan could have been avoided.

## Limits and safety

| Limit | Default | Error |
|---|---|---|
| query size | 1 MiB | `LimitExceeded` |
| expression nesting | 64 levels | `Syntax` |
| traversal depth | 64 | `LimitExceeded` |
| records visited per query | 2·10⁹ | `LimitExceeded` |
| result rows | 10⁷ | `LimitExceeded` |
| timeout | none (set per query or per graph) | `Timeout` |

* Cancellation is cooperative through `CancellationToken` / `CancelToken`.
* `read_only=True` rejects any statement that could modify the graph, which
  suits AI agents and untrusted callers.
* There are no procedures, no file or network access and no dynamic code.
