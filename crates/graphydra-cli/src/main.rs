//! `graphydra` command-line tool.
//!
//! Exit codes: 0 success, 1 internal/I-O error, 2 usage error, 3 query
//! error, 4 invalid graph file, 5 graph locked by another writer, 6 not found.

use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use graphydra_core::interop::{self, ImportReport};
use graphydra_core::language::parse_script;
use graphydra_core::{
    Graph, GraphError, IndexDef, IndexKind, IndexTarget, OpenOptions, QueryOptions, QueryResult, ResultValue, Value,
    validate_file,
};

#[derive(Parser)]
#[command(name = "graphydra", version, about = "Local-first graph database: inspect, query and maintain .graph files")]
struct Cli {
    /// Emit machine-readable JSON (results and errors).
    #[arg(long, global = true)]
    json: bool,
    /// Diagnostics printed to stderr.
    #[arg(long, global = true, value_enum, default_value_t = LogLevel::Warn)]
    log_level: LogLevel,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

#[derive(Subcommand)]
enum Command {
    /// Create an empty graph file.
    Init {
        file: PathBuf,
        /// Logical graph id (defaults to the file name).
        #[arg(long)]
        graph_id: Option<String>,
    },
    /// Validate structure, references, journal and indexes (read-only).
    Validate { file: PathBuf },
    /// Show metadata, counts, labels, types and indexes.
    Info { file: PathBuf },
    /// Execute a GHQL statement (or a script with --file).
    Query(QueryArgs),
    /// Show the execution plan without running the query.
    Explain(QueryArgs),
    /// Manage secondary indexes.
    Index {
        #[command(subcommand)]
        action: IndexAction,
    },
    /// Import data into a graph (created if missing).
    Import {
        file: PathBuf,
        #[arg(long, value_enum)]
        format: DataFormat,
        /// Input file (JSON / JSON Lines).
        input: Option<PathBuf>,
        /// Nodes CSV (csv format).
        #[arg(long)]
        nodes: Option<PathBuf>,
        /// Edges CSV (csv format).
        #[arg(long)]
        edges: Option<PathBuf>,
    },
    /// Export a graph.
    Export {
        file: PathBuf,
        #[arg(long, value_enum)]
        format: ExportFormat,
        /// Output file; for csv, a prefix producing <prefix>.nodes.csv and <prefix>.edges.csv.
        #[arg(long, short)]
        output: PathBuf,
    },
    /// Fold the journal into the snapshot and compact storage.
    Checkpoint { file: PathBuf },
    /// Interactive GHQL shell.
    Shell {
        file: PathBuf,
        /// Open without the writer lock.
        #[arg(long)]
        read_only: bool,
    },
}

#[derive(Args)]
struct QueryArgs {
    file: PathBuf,
    /// Statement text (omit when using --file).
    query: Option<String>,
    /// Read statements from a script file (`;`-separated).
    #[arg(long = "file", short = 'f')]
    script: Option<PathBuf>,
    /// Parameter as name=value; value is parsed as JSON, else taken as a string.
    #[arg(long = "param", short = 'p')]
    params: Vec<String>,
    /// Abort after this many seconds.
    #[arg(long)]
    timeout: Option<f64>,
    /// Refuse statements that modify the graph.
    #[arg(long)]
    read_only: bool,
}

#[derive(Subcommand)]
enum IndexAction {
    /// List indexes.
    List { file: PathBuf },
    /// Rebuild all indexes from records and verify consistency.
    Rebuild { file: PathBuf },
    /// Verify indexes against records (read-only).
    Verify { file: PathBuf },
    /// Create an index.
    Create(IndexArgs),
    /// Drop an index.
    Drop(IndexArgs),
}

#[derive(Args)]
struct IndexArgs {
    file: PathBuf,
    property: String,
    #[arg(long, value_enum, default_value_t = KindArg::Hash)]
    kind: KindArg,
    #[arg(long, value_enum, default_value_t = TargetArg::Node)]
    target: TargetArg,
}

#[derive(Clone, Copy, ValueEnum)]
enum KindArg {
    Hash,
    Sorted,
    Text,
}

#[derive(Clone, Copy, ValueEnum)]
enum TargetArg {
    Node,
    Edge,
}

#[derive(Clone, Copy, ValueEnum)]
enum DataFormat {
    Json,
    Jsonl,
    Csv,
}

#[derive(Clone, Copy, ValueEnum)]
enum ExportFormat {
    Json,
    Jsonl,
    Csv,
    Graphml,
}

/// CLI-level error with an exit code.
struct Failure {
    kind: &'static str,
    message: String,
    code: u8,
}

impl From<GraphError> for Failure {
    fn from(e: GraphError) -> Self {
        let (kind, code) = match &e {
            GraphError::Syntax(_) => ("syntax_error", 3),
            GraphError::Semantic(_) | GraphError::Type(_) | GraphError::Parameter(_) => ("query_error", 3),
            GraphError::Timeout | GraphError::Cancelled | GraphError::LimitExceeded(_) => ("limit_exceeded", 3),
            GraphError::Format(_) | GraphError::UnsupportedVersion { .. } | GraphError::Corrupt(_) => {
                ("invalid_file", 4)
            }
            GraphError::Validation(_) | GraphError::DanglingEdge { .. } | GraphError::DuplicateId { .. } => {
                ("validation_error", 4)
            }
            GraphError::Locked { .. } => ("locked", 5),
            GraphError::NotFound { .. } => ("not_found", 6),
            GraphError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => ("not_found", 6),
            GraphError::ReadOnly => ("read_only", 3),
            _ => ("error", 1),
        };
        Failure { kind, message: e.to_string(), code }
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Failure { kind: "io_error", message: e.to_string(), code: 1 }
    }
}

fn usage(msg: impl Into<String>) -> Failure {
    Failure { kind: "usage_error", message: msg.into(), code: 2 }
}

type CliResult = Result<(), Failure>;

struct Ctx {
    json: bool,
    log: LogLevel,
}

impl Ctx {
    fn info(&self, msg: &str) {
        if self.log >= LogLevel::Info {
            eprintln!("info: {msg}");
        }
    }

    fn warn(&self, msg: &str) {
        if self.log >= LogLevel::Warn {
            eprintln!("warning: {msg}");
        }
    }

    fn emit(&self, value: &impl serde::Serialize) -> CliResult {
        let s = serde_json::to_string_pretty(value).map_err(|e| usage(e.to_string()))?;
        println!("{s}");
        Ok(())
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let ctx = Ctx { json: cli.json, log: cli.log_level };
    match run(&ctx, cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            if ctx.json {
                let body = serde_json::json!({ "error": { "kind": f.kind, "message": f.message } });
                eprintln!("{body}");
            } else {
                eprintln!("error: {}", f.message);
            }
            ExitCode::from(f.code)
        }
    }
}

fn open_writable(ctx: &Ctx, file: &Path, create: bool) -> Result<Graph, Failure> {
    let g = Graph::open_with(file, OpenOptions::new().create(create))?;
    report_recovery(ctx, &g);
    Ok(g)
}

fn open_read(ctx: &Ctx, file: &Path) -> Result<Graph, Failure> {
    let g = Graph::open_read_only(file)?;
    report_recovery(ctx, &g);
    Ok(g)
}

fn report_recovery(ctx: &Ctx, g: &Graph) {
    let s = g.stats();
    if s.recovered_transactions > 0 {
        ctx.info(&format!("replayed {} committed transaction(s) from the journal", s.recovered_transactions));
    }
    if s.recovered_torn_write {
        ctx.warn("discarded an incomplete journal record left by an interrupted write");
    }
}

fn run(ctx: &Ctx, cmd: Command) -> CliResult {
    match cmd {
        Command::Init { file, graph_id } => {
            if file.exists() {
                return Err(usage(format!("{} already exists", file.display())));
            }
            let mut opts = OpenOptions::new().create(true);
            opts.graph_id = graph_id;
            let g = Graph::open_with(&file, opts)?;
            let id = g.graph_id().to_owned();
            g.close()?;
            if ctx.json {
                ctx.emit(&serde_json::json!({ "created": file, "graph_id": id }))
            } else {
                println!("created {} (graph_id {id:?})", file.display());
                Ok(())
            }
        }
        Command::Validate { file } => {
            let report = validate_file(&file);
            if ctx.json {
                ctx.emit(&report)?;
            } else {
                for e in &report.errors {
                    println!("ERROR   {e}");
                }
                for w in &report.warnings {
                    println!("WARNING {w}");
                }
                if report.valid {
                    println!(
                        "valid: {} node(s), {} edge(s), generation {}",
                        report.nodes, report.edges, report.generation
                    );
                }
            }
            if report.valid {
                Ok(())
            } else {
                Err(Failure { kind: "invalid_file", message: format!("{} is not valid", file.display()), code: 4 })
            }
        }
        Command::Info { file } => {
            let g = open_read(ctx, &file)?;
            let stats = g.stats();
            if ctx.json {
                return ctx.emit(&serde_json::json!({ "path": file, "metadata": g.metadata(), "stats": stats }));
            }
            println!("file:        {}", file.display());
            println!("graph_id:    {}", stats.graph_id);
            println!("generation:  {}", stats.generation);
            println!("nodes:       {}", stats.nodes);
            println!("edges:       {}", stats.edges);
            println!("journal:     {} byte(s) pending checkpoint", stats.journal_bytes);
            for (k, v) in g.metadata() {
                println!("meta.{k}: {v}");
            }
            if !stats.labels.is_empty() {
                println!("labels:");
                for (l, c) in &stats.labels {
                    println!("  :{l:<24} {c}");
                }
            }
            if !stats.edge_types.is_empty() {
                println!("edge types:");
                for (t, c) in &stats.edge_types {
                    println!("  :{t:<24} {c}");
                }
            }
            print_indexes(&g);
            Ok(())
        }
        Command::Query(args) => run_query(ctx, args, false),
        Command::Explain(args) => run_query(ctx, args, true),
        Command::Index { action } => run_index(ctx, action),
        Command::Import { file, format, input, nodes, edges } => {
            let mut g = open_writable(ctx, &file, true)?;
            let open_input = |p: &Option<PathBuf>, what: &str| -> Result<BufReader<File>, Failure> {
                let p = p.as_ref().ok_or_else(|| usage(format!("missing {what}")))?;
                Ok(BufReader::new(File::open(p).map_err(|e| Failure {
                    kind: "not_found",
                    message: format!("{}: {e}", p.display()),
                    code: 6,
                })?))
            };
            let report: ImportReport = match format {
                DataFormat::Json => interop::import_json(&mut g, open_input(&input, "input file")?)?,
                DataFormat::Jsonl => interop::import_jsonl(&mut g, open_input(&input, "input file")?)?,
                DataFormat::Csv => {
                    if nodes.is_none() && edges.is_none() {
                        return Err(usage("csv import needs --nodes and/or --edges"));
                    }
                    let n = nodes.as_ref().map(|_| open_input(&nodes, "--nodes")).transpose()?;
                    let e = edges.as_ref().map(|_| open_input(&edges, "--edges")).transpose()?;
                    interop::import_csv(&mut g, n, e)?
                }
            };
            g.close()?;
            if ctx.json {
                ctx.emit(&report)
            } else {
                println!("imported {} node(s) and {} edge(s)", report.nodes, report.edges);
                Ok(())
            }
        }
        Command::Export { file, format, output } => {
            let g = open_read(ctx, &file)?;
            let create = |p: &Path| {
                File::create(p).map_err(|e| Failure::from(io::Error::new(e.kind(), format!("{}: {e}", p.display()))))
            };
            match format {
                ExportFormat::Json => interop::export_json(&g, &mut create(&output)?)?,
                ExportFormat::Jsonl => interop::export_jsonl(&g, &mut create(&output)?)?,
                ExportFormat::Graphml => interop::export_graphml(&g, &mut create(&output)?)?,
                ExportFormat::Csv => {
                    let base = output.display().to_string();
                    let (np, ep) =
                        (PathBuf::from(format!("{base}.nodes.csv")), PathBuf::from(format!("{base}.edges.csv")));
                    interop::export_csv(&g, &mut create(&np)?, &mut create(&ep)?)?;
                }
            }
            if !ctx.json {
                println!("exported {} node(s) and {} edge(s)", g.node_count(), g.edge_count());
            }
            Ok(())
        }
        Command::Checkpoint { file } => {
            let mut g = open_writable(ctx, &file, false)?;
            g.checkpoint()?;
            let stats = g.stats();
            g.close()?;
            if ctx.json {
                ctx.emit(
                    &serde_json::json!({ "generation": stats.generation, "nodes": stats.nodes, "edges": stats.edges }),
                )
            } else {
                println!("checkpointed generation {}", stats.generation);
                Ok(())
            }
        }
        Command::Shell { file, read_only } => shell(ctx, &file, read_only),
    }
}

fn parse_params(raw: &[String]) -> Result<QueryOptions, Failure> {
    let mut opts = QueryOptions::new();
    for p in raw {
        let (name, value) =
            p.split_once('=').ok_or_else(|| usage(format!("--param {p:?} must look like name=value")))?;
        let value: Value = serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_owned()));
        opts.parameters.insert(name.trim().trim_start_matches('$').to_owned(), value);
    }
    Ok(opts)
}

fn run_query(ctx: &Ctx, args: QueryArgs, explain_only: bool) -> CliResult {
    let text = match (&args.query, &args.script) {
        (Some(q), None) => q.clone(),
        (None, Some(path)) => std::fs::read_to_string(path).map_err(|e| usage(format!("{}: {e}", path.display())))?,
        _ => return Err(usage("give either a query argument or --file")),
    };
    let mut opts = parse_params(&args.params)?;
    opts.read_only = args.read_only;
    if let Some(t) = args.timeout {
        if !(t.is_finite() && t > 0.0) {
            return Err(usage("--timeout must be a positive number of seconds"));
        }
        opts.timeout = Some(Duration::from_secs_f64(t));
    }
    let statements = parse_script(&text)?;
    if statements.is_empty() {
        return Err(usage("no statement given"));
    }
    let mutates = !explain_only && statements.iter().any(|s| s.is_mutation());
    let mut g =
        if mutates && !args.read_only { open_writable(ctx, &args.file, false)? } else { open_read(ctx, &args.file)? };
    if explain_only {
        let plan = g.explain(&text, &opts)?;
        return if ctx.json {
            ctx.emit(&plan)
        } else {
            print!("{}", plan.to_text());
            Ok(())
        };
    }
    let results = if statements.len() == 1 { vec![g.query(&text, &opts)?] } else { g.execute_script(&text, &opts)? };
    if mutates {
        g.close()?;
    }
    if ctx.json {
        return if results.len() == 1 { ctx.emit(&results[0]) } else { ctx.emit(&results) };
    }
    for r in &results {
        print_result(r);
    }
    Ok(())
}

fn run_index(ctx: &Ctx, action: IndexAction) -> CliResult {
    match action {
        IndexAction::List { file } => {
            let g = open_read(ctx, &file)?;
            if ctx.json {
                ctx.emit(&g.indexes())
            } else {
                print_indexes(&g);
                Ok(())
            }
        }
        IndexAction::Verify { file } => {
            let g = open_read(ctx, &file)?;
            finish_verify(ctx, &g.verify())
        }
        IndexAction::Rebuild { file } => {
            let mut g = open_writable(ctx, &file, false)?;
            g.rebuild_indexes();
            let problems = g.verify();
            g.close()?;
            finish_verify(ctx, &problems)
        }
        IndexAction::Create(a) => {
            let mut g = open_writable(ctx, &a.file, false)?;
            g.create_index(index_def(&a))?;
            g.close()?;
            println!("created {}", index_def(&a));
            Ok(())
        }
        IndexAction::Drop(a) => {
            let mut g = open_writable(ctx, &a.file, false)?;
            g.drop_index(index_def(&a))?;
            g.close()?;
            println!("dropped {}", index_def(&a));
            Ok(())
        }
    }
}

fn index_def(a: &IndexArgs) -> IndexDef {
    let kind = match a.kind {
        KindArg::Hash => IndexKind::Hash,
        KindArg::Sorted => IndexKind::Sorted,
        KindArg::Text => IndexKind::Text,
    };
    let target = match a.target {
        TargetArg::Node => IndexTarget::Node,
        TargetArg::Edge => IndexTarget::Edge,
    };
    IndexDef::new(kind, target, a.property.clone())
}

fn finish_verify(ctx: &Ctx, problems: &[String]) -> CliResult {
    if ctx.json {
        ctx.emit(&serde_json::json!({ "consistent": problems.is_empty(), "problems": problems }))?;
    } else if problems.is_empty() {
        println!("all indexes are consistent with the records");
    } else {
        for p in problems {
            println!("PROBLEM {p}");
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Failure { kind: "invalid_file", message: "index verification failed".into(), code: 4 })
    }
}

fn print_indexes(g: &Graph) {
    let idx = g.indexes();
    if idx.is_empty() {
        println!("indexes:     (none besides id, label, type and adjacency)");
        return;
    }
    println!("indexes:");
    for i in idx {
        println!("  {:<40} {} entries, {} keys", i.def.to_string(), i.entries, i.distinct_keys);
    }
}

fn display(v: &ResultValue) -> String {
    match v {
        ResultValue::Null => "null".into(),
        ResultValue::String(s) => s.clone(),
        ResultValue::Node(n) => {
            let labels: String = n.labels.iter().map(|l| format!(":{l}")).collect();
            let props = serde_json::to_string(&n.properties).unwrap_or_default();
            format!("({}{labels} {props})", n.id)
        }
        ResultValue::Edge(e) => format!("[{}:{} {}->{}]", e.id, e.edge_type, e.source, e.target),
        ResultValue::Path(p) => {
            let mut s = String::new();
            for (i, n) in p.nodes.iter().enumerate() {
                if i > 0 {
                    s.push_str(&format!("-[{}]-", p.edges[i - 1].edge_type));
                }
                s.push_str(&format!("({})", n.id));
            }
            s
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn print_result(r: &QueryResult) {
    if r.columns.is_empty() {
        let s = &r.stats;
        if s.contains_updates() {
            println!(
                "ok: {} node(s) created, {} deleted; {} edge(s) created, {} deleted; {} property update(s); {} label(s) added, {} removed",
                s.nodes_created,
                s.nodes_deleted,
                s.edges_created,
                s.edges_deleted,
                s.properties_set,
                s.labels_added,
                s.labels_removed
            );
        } else {
            println!("ok");
        }
        return;
    }
    let cells: Vec<Vec<String>> = r.rows.iter().map(|row| row.iter().map(display).collect()).collect();
    let mut widths: Vec<usize> = r.columns.iter().map(|c| c.chars().count()).collect();
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count()).min(60);
        }
    }
    let line = |vals: &[String]| {
        let parts: Vec<String> = vals
            .iter()
            .zip(&widths)
            .map(|(v, w)| {
                let v: String =
                    if v.chars().count() > *w { v.chars().take(w - 1).chain(['…']).collect() } else { v.clone() };
                format!("{v:<w$}", w = *w)
            })
            .collect();
        println!("{}", parts.join(" | ").trim_end());
    };
    line(&r.columns);
    println!("{}", widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-"));
    for row in &cells {
        line(row);
    }
    println!(
        "({} row{}, {:.2} ms)",
        r.rows.len(),
        if r.rows.len() == 1 { "" } else { "s" },
        r.stats.execution_time.as_secs_f64() * 1e3
    );
}

fn shell(ctx: &Ctx, file: &Path, read_only: bool) -> CliResult {
    let mut g = if read_only { open_read(ctx, file)? } else { open_writable(ctx, file, false)? };
    let interactive = io::stdin().is_terminal();
    if interactive {
        println!(
            "GrapHydra {}: {} ({} nodes, {} edges)",
            graphydra_core::VERSION,
            file.display(),
            g.node_count(),
            g.edge_count()
        );
        println!("End statements with ';'. Commands: :help :info :begin :commit :rollback :quit");
    }
    let stdin = io::stdin();
    let mut buffer = String::new();
    let prompt = |cont: bool| {
        if interactive {
            print!("{}", if cont { "   ...> " } else { "ghql> " });
            let _ = io::stdout().flush();
        }
    };
    prompt(false);
    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if buffer.is_empty() && trimmed.starts_with(':') {
            match trimmed {
                ":quit" | ":q" | ":exit" => break,
                ":help" => println!("Statements end with ';'. Example: MATCH (n) RETURN n LIMIT 5;"),
                ":info" => {
                    println!("{} node(s), {} edge(s), generation {}", g.node_count(), g.edge_count(), g.generation())
                }
                ":begin" => report(g.begin()),
                ":commit" => report(g.commit()),
                ":rollback" => report(g.rollback()),
                other => println!("unknown command {other}"),
            }
            prompt(false);
            continue;
        }
        buffer.push_str(&line);
        buffer.push('\n');
        if !trimmed.ends_with(';') {
            prompt(true);
            continue;
        }
        let text = std::mem::take(&mut buffer);
        match g.execute_script(&text, &QueryOptions::new()) {
            Ok(results) => results.iter().for_each(print_result),
            Err(e) => println!("error: {e}"),
        }
        prompt(false);
    }
    if g.in_transaction() {
        println!("rolling back the open transaction");
    }
    if read_only { Ok(()) } else { g.close().map_err(Failure::from) }
}

fn report(r: graphydra_core::Result<()>) {
    match r {
        Ok(()) => println!("ok"),
        Err(e) => println!("error: {e}"),
    }
}
