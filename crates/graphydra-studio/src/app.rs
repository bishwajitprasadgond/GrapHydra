//! Application state, layout, and all user actions.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, RichText, TextEdit, Ui};
use egui_extras::{Column, TableBuilder};
use graphydra_core::{
    CancellationToken, Direction, Edge, Graph, GraphError, GraphStats, IndexDef, IndexKind, IndexTarget, Node,
    Properties, QueryOptions, QueryResult, ResultValue, Value, interop, validate_file,
};

use crate::graph_view::{self, CanvasAction, GraphView, Selection};
use crate::settings::Settings;
use crate::theme::{self, Palette};

const RUN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL, Key::Enter);
const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL, Key::S);
const OPEN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL, Key::O);
const NEW_NODE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL, Key::N);
const NEW_EDGE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL, Key::E);
const LOAD_LIMIT: usize = 300;
const EXPAND_LIMIT: usize = 200;
const PAGE: usize = 100;

/// Examples menu: (name, query, parameters template).
const SAMPLES: &[(&str, &str, &str)] = &[
    ("All nodes (first 100)", "MATCH (n) RETURN n LIMIT 100", "{}"),
    ("Relationships", "MATCH (a)-[r]->(b) RETURN a, r, b LIMIT 200", "{}"),
    ("Count by labels", "MATCH (n) RETURN labels(n) AS labels, count(*) AS total ORDER BY total DESC", "{}"),
    ("Two-hop neighborhood", "MATCH (a {id: $id})-[*1..2]-(b) RETURN DISTINCT b LIMIT 100", r#"{"id": "u-alice"}"#),
    (
        "Shortest path",
        "MATCH p = shortestPath((a {id: $from})-[*..6]-(b {id: $to})) RETURN p",
        r#"{"from": "u-alice", "to": "d-1"}"#,
    ),
    ("Create a node", "CREATE (n:Person {name: \"Ada\", role: \"engineer\"}) RETURN n", "{}"),
    (
        "Connect two nodes",
        "MATCH (a {id: $from}), (b {id: $to}) CREATE (a)-[r:KNOWS]->(b) RETURN a, r, b",
        r#"{"from": "u-alice", "to": "u-bob"}"#,
    ),
    ("Update a property", "MATCH (n {id: $id}) SET n.reviewed = true RETURN n", r#"{"id": "web-01"}"#),
    ("Delete a node", "MATCH (n {id: $id}) DETACH DELETE n", r#"{"id": "n-to-delete"}"#),
];

/// Help window examples; they run against the sample database.
const GUIDE: &[(&str, &str)] = &[
    ("Find", "MATCH (n:Host {os: \"linux\"}) RETURN n"),
    ("Filter", "MATCH (n:User) WHERE n.department = \"finance\" AND n.name STARTS WITH \"a\" RETURN n"),
    ("Relationships", "MATCH (u:User)-[r:LOGGED_INTO]->(h:Host) RETURN u.name, h.hostname, r.at"),
    ("Multi-hop", "MATCH (a {id: \"u-alice\"})-[*1..3]->(b) RETURN DISTINCT b"),
    ("Aggregate", "MATCH (h:Host)<-[:LOGGED_INTO]-(u) RETURN h.hostname, count(u) AS users ORDER BY users DESC"),
    ("Full-text", "MATCH (n) WHERE text_match(n.name, \"powershell\") RETURN n"),
    ("Create", "CREATE (a:Person {name: \"Ada\"})-[:KNOWS]->(b:Person {name: \"Grace\"}) RETURN a, b"),
    ("Update", "MATCH (n:Host {hostname: \"web-01\"}) SET n.patched = true, n:Reviewed RETURN n"),
    ("Delete", "MATCH (n:Person {name: \"Ada\"}) DETACH DELETE n"),
    ("Indexes", "CREATE SORTED INDEX ON NODES(cvss)"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Graph,
    Query,
    Data,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Info,
    Success,
    Error,
}

struct Status {
    text: String,
    kind: StatusKind,
}

struct Db {
    graph: Arc<Mutex<Graph>>,
    path: PathBuf,
    read_only: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunMode {
    Run,
    Explain,
    Profile,
}

struct Running {
    rx: Receiver<Result<QueryResult, GraphError>>,
    cancel: CancellationToken,
    started: Instant,
    mode: RunMode,
}

#[derive(Default)]
struct PropRows(Vec<(String, String)>);

impl PropRows {
    fn from(props: &Properties) -> Self {
        PropRows(props.iter().map(|(k, v)| (k.clone(), value_to_input(v))).collect())
    }

    /// Parse rows into properties. Values are JSON if they parse, else strings.
    fn parse(&self) -> Result<Properties, String> {
        let mut out = Properties::new();
        for (k, v) in &self.0 {
            let k = k.trim();
            if k.is_empty() {
                if v.trim().is_empty() {
                    continue;
                }
                return Err("every property needs a name".into());
            }
            out.insert(k.to_string(), input_to_value(v)?);
        }
        Ok(out)
    }

    fn ui(&mut self, ui: &mut Ui, id: &str) {
        let mut remove = None;
        // A row must never be wider than the panel: the panel grows to fit its
        // content, so an overflowing row would widen it a little every frame.
        // The button goes first (right to left) and the value takes what is left.
        let row_h = ui.spacing().interact_size.y;
        let name_w = (ui.available_width() * 0.35).max(60.0);
        ui.push_id(id, |ui| {
            for (i, (k, v)) in self.0.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_sized([name_w, row_h], TextEdit::singleline(k).hint_text("name"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("×").on_hover_text("Remove property").clicked() {
                            remove = Some(i);
                        }
                        let value_w = ui.available_width().max(40.0);
                        ui.add_sized([value_w, row_h], TextEdit::singleline(v).hint_text("value"));
                    });
                });
            }
        });
        if let Some(i) = remove {
            self.0.remove(i);
        }
        if ui.small_button("+ Add property").clicked() {
            self.0.push((String::new(), String::new()));
        }
    }
}

fn value_to_input(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn input_to_value(s: &str) -> Result<Value, String> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(Value::String(String::new()));
    }
    match serde_json::from_str::<Value>(t) {
        Ok(v) => Ok(v),
        // Text that merely looks structured is more likely a typo than a string.
        Err(e) if t.starts_with('{') || t.starts_with('[') => Err(format!("invalid JSON value `{t}`: {e}")),
        Err(_) => Ok(Value::String(s.to_string())),
    }
}

#[derive(Default)]
struct NodeForm {
    id: String,
    labels: String,
    props: PropRows,
}

struct EdgeForm {
    id: String,
    source: String,
    target: String,
    edge_type: String,
    directed: bool,
    props: PropRows,
}

impl Default for EdgeForm {
    fn default() -> Self {
        EdgeForm {
            id: String::new(),
            source: String::new(),
            target: String::new(),
            edge_type: String::new(),
            directed: true,
            props: PropRows::default(),
        }
    }
}

struct IndexForm {
    property: String,
    kind: IndexKind,
    target: IndexTarget,
}

#[derive(Default)]
struct Inspector {
    for_sel: Option<Selection>,
    props: PropRows,
    new_label: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DataKind {
    Nodes,
    Edges,
}

struct DataBrowser {
    kind: DataKind,
    label: String,
    search: String,
    page: usize,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    loaded: bool,
}

enum Confirm {
    DeleteNode(String),
    DeleteEdge(String),
    CloseWithTransaction,
}

pub struct StudioApp {
    db: Option<Db>,
    stats: Option<GraphStats>,
    settings: Settings,
    tab: Tab,
    view: GraphView,
    label_slots: BTreeMap<String, usize>,
    // Query workspace.
    query: String,
    params: String,
    timeout_secs: u32,
    query_read_only: bool,
    running: Option<Running>,
    result: Option<QueryResult>,
    result_mode: RunMode,
    query_error: Option<String>,
    // Forms and dialogs.
    node_form: Option<NodeForm>,
    edge_form: Option<EdgeForm>,
    index_form: IndexForm,
    inspector: Inspector,
    data: DataBrowser,
    confirm: Option<Confirm>,
    show_help: bool,
    show_about: bool,
    status: Status,
    pending_fit: bool,
}

fn lock(g: &Arc<Mutex<Graph>>) -> MutexGuard<'_, Graph> {
    g.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl StudioApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        let settings = Settings::load();
        theme::apply(&cc.egui_ctx, settings.dark);
        let mut app = StudioApp {
            db: None,
            stats: None,
            settings,
            tab: Tab::Graph,
            view: GraphView::default(),
            label_slots: BTreeMap::new(),
            query: "MATCH (n) RETURN n LIMIT 100".into(),
            params: "{}".into(),
            timeout_secs: 30,
            query_read_only: false,
            running: None,
            result: None,
            result_mode: RunMode::Run,
            query_error: None,
            node_form: None,
            edge_form: None,
            index_form: IndexForm { property: String::new(), kind: IndexKind::Hash, target: IndexTarget::Node },
            inspector: Inspector::default(),
            data: DataBrowser {
                kind: DataKind::Nodes,
                label: String::new(),
                search: String::new(),
                page: 0,
                nodes: Vec::new(),
                edges: Vec::new(),
                loaded: false,
            },
            confirm: None,
            show_help: false,
            show_about: false,
            status: Status { text: "Open or create a database to begin (File menu).".into(), kind: StatusKind::Info },
            pending_fit: false,
        };
        if let Some(path) = initial {
            app.open_path(&path, false);
        }
        app
    }

    fn pal(&self) -> Palette {
        theme::palette(self.settings.dark)
    }

    fn info(&mut self, t: impl Into<String>) {
        self.status = Status { text: t.into(), kind: StatusKind::Info };
    }

    fn ok(&mut self, t: impl Into<String>) {
        self.status = Status { text: t.into(), kind: StatusKind::Success };
    }

    fn err(&mut self, t: impl Into<String>) {
        self.status = Status { text: t.into(), kind: StatusKind::Error };
    }

    /// Run `f` against the open graph. Reports "busy" while a query runs.
    fn with_graph<T>(&mut self, f: impl FnOnce(&mut Graph) -> Result<T, GraphError>) -> Option<T> {
        let Some(db) = &self.db else {
            self.err("No database is open.");
            return None;
        };
        let graph = Arc::clone(&db.graph);
        let Ok(mut g) = graph.try_lock() else {
            self.err("A query is still running. Wait for it or press Cancel.");
            return None;
        };
        match f(&mut g) {
            Ok(v) => Some(v),
            Err(e) => {
                drop(g);
                self.err(e.to_string());
                None
            }
        }
    }

    fn refresh_stats(&mut self) {
        let Some(db) = &self.db else {
            self.stats = None;
            return;
        };
        if let Ok(g) = db.graph.try_lock() {
            let stats = g.stats();
            let mut labels: Vec<&String> = stats.labels.iter().map(|(l, _)| l).collect();
            labels.sort();
            self.label_slots = labels.into_iter().enumerate().map(|(i, l)| (l.clone(), i)).collect();
            self.stats = Some(stats);
        }
    }

    // ----- file operations -------------------------------------------------

    fn dialog() -> rfd::FileDialog {
        rfd::FileDialog::new().add_filter("GrapHydra database", &["graph"]).add_filter("All files", &["*"])
    }

    fn can_switch_db(&mut self) -> bool {
        if self.running.is_some() {
            self.err("Cancel the running query first.");
            return false;
        }
        true
    }

    fn new_db(&mut self, demo: bool) {
        if !self.can_switch_db() {
            return;
        }
        let name = if demo { "sample.graph" } else { "untitled.graph" };
        let Some(path) = Self::dialog().set_title("Create database").set_file_name(name).save_file() else { return };
        let path = with_graph_ext(path);
        if path.exists() {
            // The OS dialog already confirmed overwrite; GrapHydra never silently replaces a file.
            if let Err(e) = std::fs::remove_file(&path) {
                self.err(format!("Cannot replace {}: {e}", path.display()));
                return;
            }
            let _ = std::fs::remove_file(journal_path(&path));
        }
        self.close_db();
        match Graph::create(&path) {
            Ok(mut g) => {
                if demo && let Err(e) = crate::demo::populate(&mut g) {
                    self.err(format!("Sample data failed: {e}"));
                }
                self.attach(g, path.clone(), false);
                self.ok(format!("Created {}", path.display()));
                if demo {
                    self.load_overview();
                }
            }
            Err(e) => self.err(format!("Cannot create database: {e}")),
        }
    }

    fn open_dialog(&mut self, read_only: bool) {
        if !self.can_switch_db() {
            return;
        }
        if let Some(path) = Self::dialog().set_title("Open database").pick_file() {
            self.open_path(&path, read_only);
        }
    }

    fn open_path(&mut self, path: &Path, read_only: bool) {
        self.close_db();
        let opened = if read_only { Graph::open_read_only(path) } else { Graph::open(path) };
        match opened {
            Ok(g) => {
                let recovered = g.stats().recovered_transactions;
                self.attach(g, path.to_path_buf(), read_only);
                let mode = if read_only { " (read-only)" } else { "" };
                if recovered > 0 {
                    self.ok(format!(
                        "Opened {}{mode}; recovered {recovered} committed transaction(s).",
                        path.display()
                    ));
                } else {
                    self.ok(format!("Opened {}{mode}", path.display()));
                }
                self.load_overview();
            }
            Err(GraphError::Locked { .. }) if !read_only => {
                // Another process is writing; a read-only view is still useful.
                self.open_path(path, true);
                if self.db.is_some() {
                    self.info("The file is locked by another writer, so it was opened read-only.");
                }
            }
            Err(e) => self.err(format!("Cannot open {}: {e}", path.display())),
        }
    }

    fn attach(&mut self, g: Graph, path: PathBuf, read_only: bool) {
        self.settings.remember_file(&path);
        self.db = Some(Db { graph: Arc::new(Mutex::new(g)), path, read_only });
        self.view.clear();
        self.result = None;
        self.query_error = None;
        self.data.loaded = false;
        self.inspector = Inspector::default();
        self.refresh_stats();
    }

    fn close_db(&mut self) {
        let Some(db) = self.db.take() else { return };
        self.view.clear();
        self.stats = None;
        self.result = None;
        self.data.loaded = false;
        match Arc::try_unwrap(db.graph) {
            Ok(m) => {
                let g = m.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Err(e) = g.close() {
                    self.err(format!("Closed with an error (data is safe in the journal): {e}"));
                }
            }
            // A worker still holds it; it is released when the worker ends.
            Err(_) => self.info("Database closed."),
        }
    }

    fn request_close(&mut self) {
        let in_tx = self.db.as_ref().is_some_and(|d| d.graph.try_lock().is_ok_and(|g| g.in_transaction()));
        if in_tx {
            self.confirm = Some(Confirm::CloseWithTransaction);
        } else if self.can_switch_db() {
            self.close_db();
            self.info("Database closed.");
        }
    }

    fn save(&mut self) {
        if self.db.as_ref().is_some_and(|d| d.read_only) {
            self.err("The database is open read-only.");
            return;
        }
        if self.with_graph(Graph::checkpoint).is_some() {
            self.ok("Saved. All committed changes are in the main file.");
            self.refresh_stats();
        }
    }

    fn save_as(&mut self) {
        let Some(path) = Self::dialog().set_title("Save a copy as").save_file() else { return };
        let path = with_graph_ext(path);
        if self.with_graph(|g| g.save_as(&path)).is_some() {
            self.ok(format!("Saved a copy to {}", path.display()));
        }
    }

    fn validate(&mut self) {
        let Some(path) = self.db.as_ref().map(|d| d.path.clone()) else { return };
        let report = validate_file(&path);
        if !report.valid {
            return self.err(format!("Invalid file: {}", report.errors.join("; ")));
        }
        let checks = self.with_graph(|g| Ok(g.verify())).unwrap_or_default();
        if checks.is_empty() {
            let pending = match report.pending_journal_records {
                0 => String::new(),
                n => format!(" {n} committed change(s) are in the journal until the next save."),
            };
            self.ok(format!(
                "Valid: {} nodes, {} relationships; indexes consistent.{pending}",
                report.nodes, report.edges
            ));
        } else {
            self.err(format!("Index check found problems: {}", checks.join("; ")));
        }
    }

    fn import(&mut self, format: &str) {
        let pick =
            |title: &str, ext: &[&str]| rfd::FileDialog::new().set_title(title).add_filter(ext[0], ext).pick_file();
        let result = match format {
            "json" => pick("Import JSON", &["json"]).map(|p| {
                File::open(&p).map_err(|e| e.to_string()).and_then(|f| {
                    self.with_graph(|g| interop::import_json(g, BufReader::new(f))).ok_or_else(String::new)
                })
            }),
            "jsonl" => pick("Import JSON Lines", &["jsonl", "ndjson"]).map(|p| {
                File::open(&p).map_err(|e| e.to_string()).and_then(|f| {
                    self.with_graph(|g| interop::import_jsonl(g, BufReader::new(f))).ok_or_else(String::new)
                })
            }),
            _ => {
                let nodes = pick("Import CSV: nodes file (cancel to skip)", &["csv"]);
                let edges = pick("Import CSV: edges file (cancel to skip)", &["csv"]);
                if nodes.is_none() && edges.is_none() {
                    None
                } else {
                    let open = |p: &Option<PathBuf>| p.as_ref().map(File::open).transpose();
                    Some(match (open(&nodes), open(&edges)) {
                        (Ok(n), Ok(e)) => self.with_graph(|g| interop::import_csv(g, n, e)).ok_or_else(String::new),
                        (Err(e), _) | (_, Err(e)) => Err(e.to_string()),
                    })
                }
            }
        };
        match result {
            Some(Ok(r)) => {
                self.ok(format!("Imported {} nodes and {} edges.", r.nodes, r.edges));
                self.refresh_stats();
                self.data.loaded = false;
                self.load_overview();
            }
            Some(Err(e)) if !e.is_empty() => self.err(format!("Import failed: {e}")),
            _ => {}
        }
    }

    fn export(&mut self, format: &str) {
        let (title, ext) = match format {
            "json" => ("Export JSON", "json"),
            "jsonl" => ("Export JSON Lines", "jsonl"),
            "graphml" => ("Export GraphML", "graphml"),
            _ => ("Export CSV: choose a name; writes <name>.nodes.csv and <name>.edges.csv", "csv"),
        };
        let Some(path) = rfd::FileDialog::new().set_title(title).add_filter(ext, &[ext]).save_file() else { return };
        let write = |path: &Path, f: &mut dyn FnMut(&mut dyn Write) -> Result<(), GraphError>| -> Result<(), String> {
            let mut out = BufWriter::new(File::create(path).map_err(|e| e.to_string())?);
            f(&mut out).map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())
        };
        let Some(db) = &self.db else { return };
        let graph = Arc::clone(&db.graph);
        let Ok(g) = graph.try_lock() else {
            self.err("A query is still running.");
            return;
        };
        let outcome = match format {
            "json" => write(&path, &mut |w| interop::export_json(&g, w)),
            "jsonl" => write(&path, &mut |w| interop::export_jsonl(&g, w)),
            "graphml" => write(&path, &mut |w| interop::export_graphml(&g, w)),
            _ => {
                // Same naming as `graphydra export --format csv -o <prefix>`.
                let (nodes_path, edges_path) = csv_paths(&path);
                File::create(&edges_path).map_err(|e| e.to_string()).and_then(|ef| {
                    let mut ew = BufWriter::new(ef);
                    let r = write(&nodes_path, &mut |w| interop::export_csv(&g, w, &mut ew));
                    r.and_then(|()| ew.flush().map_err(|e| e.to_string()))
                })
            }
        };
        drop(g);
        let shown = if format == "csv" {
            let (n, e) = csv_paths(&path);
            format!("{} and {}", n.display(), e.display())
        } else {
            path.display().to_string()
        };
        match outcome {
            Ok(()) => self.ok(format!("Exported to {shown}")),
            Err(e) => self.err(format!("Export failed: {e}")),
        }
    }

    // ----- graph view feeding ---------------------------------------------

    fn load_overview(&mut self) {
        let loaded = self.with_graph(|g| {
            let nodes: Vec<Node> = g.nodes().take(LOAD_LIMIT).collect();
            Ok(nodes)
        });
        if let Some(nodes) = loaded {
            self.view.clear();
            self.show_nodes(nodes, None);
            self.pending_fit = true;
        }
    }

    /// Add nodes to the view and connect them to everything already shown.
    fn show_nodes(&mut self, nodes: Vec<Node>, near: Option<egui::Vec2>) {
        let ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
        for n in nodes {
            self.view.add_node(n, near);
        }
        let edges = self.with_graph(|g| {
            let mut out = Vec::new();
            for id in &ids {
                for (e, _) in g.neighbors(id, Direction::Both, None)? {
                    out.push(e);
                }
            }
            Ok(out)
        });
        for e in edges.unwrap_or_default() {
            self.view.add_edge(e);
        }
        if self.view.truncated > 0 {
            self.info(format!(
                "The view is limited to {} nodes; {} more were not drawn. Narrow the query to see them.",
                graph_view::MAX_VISIBLE_NODES,
                self.view.truncated
            ));
            self.view.truncated = 0;
        }
    }

    fn show_label(&mut self, label: &str) {
        let label = label.to_string();
        if let Some(nodes) = self.with_graph(|g| g.find_nodes(Some(&label), &Properties::new(), Some(LOAD_LIMIT))) {
            let n = nodes.len();
            self.view.clear();
            self.show_nodes(nodes, None);
            self.pending_fit = true;
            self.tab = Tab::Graph;
            self.info(format!("Showing {n} :{label} node(s)."));
        }
    }

    fn show_edge_type(&mut self, ty: &str) {
        let ty = ty.to_string();
        let found = self.with_graph(|g| {
            let mut nodes = BTreeMap::new();
            let mut edges = Vec::new();
            for e in g.edges().filter(|e| e.edge_type == ty).take(LOAD_LIMIT) {
                for id in [&e.source, &e.target] {
                    if !nodes.contains_key(id)
                        && let Some(n) = g.get_node(id)
                    {
                        nodes.insert(id.clone(), n);
                    }
                }
                edges.push(e);
            }
            Ok((nodes.into_values().collect::<Vec<_>>(), edges))
        });
        if let Some((nodes, edges)) = found {
            self.view.clear();
            for n in nodes {
                self.view.add_node(n, None);
            }
            for e in edges {
                self.view.add_edge(e);
            }
            self.pending_fit = true;
            self.tab = Tab::Graph;
            self.info(format!("Showing up to {LOAD_LIMIT} :{ty} relationship(s)."));
        }
    }

    fn expand(&mut self, id: &str) {
        let near = self.view.position_of(id);
        let id = id.to_string();
        if let Some(pairs) = self.with_graph(|g| g.neighbors(&id, Direction::Both, None)) {
            let total = pairs.len();
            let nodes: Vec<Node> = pairs.into_iter().take(EXPAND_LIMIT).map(|(_, n)| n).collect();
            self.show_nodes(nodes, near);
            if total > EXPAND_LIMIT {
                self.info(format!("{id} has {total} neighbors; showing the first {EXPAND_LIMIT}."));
            } else {
                self.info(format!("{id}: {total} neighbor(s)."));
            }
        }
    }

    fn show_result_in_graph(&mut self) {
        let Some(result) = &self.result else { return };
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        fn walk(v: &ResultValue, nodes: &mut Vec<Node>, edges: &mut Vec<Edge>) {
            match v {
                ResultValue::Node(n) => nodes.push(n.clone()),
                ResultValue::Edge(e) => edges.push(e.clone()),
                ResultValue::Path(p) => {
                    nodes.extend(p.nodes.iter().cloned());
                    edges.extend(p.edges.iter().cloned());
                }
                ResultValue::List(items) => items.iter().for_each(|i| walk(i, nodes, edges)),
                ResultValue::Map(m) => m.values().for_each(|i| walk(i, nodes, edges)),
                _ => {}
            }
        }
        for row in &result.rows {
            for v in row {
                walk(v, &mut nodes, &mut edges);
            }
        }
        if nodes.is_empty() && edges.is_empty() {
            self.info("The result has no nodes, relationships, or paths to draw.");
            return;
        }
        // Edges whose endpoints were not returned still need their nodes.
        let missing: Vec<String> = edges
            .iter()
            .flat_map(|e| [e.source.clone(), e.target.clone()])
            .filter(|id| !nodes.iter().any(|n| &n.id == id))
            .collect();
        if !missing.is_empty()
            && let Some(extra) =
                self.with_graph(|g| Ok(missing.iter().filter_map(|id| g.get_node(id)).collect::<Vec<_>>()))
        {
            nodes.extend(extra);
        }
        self.view.clear();
        self.show_nodes(nodes, None);
        for e in edges {
            self.view.add_edge(e);
        }
        self.pending_fit = true;
        self.tab = Tab::Graph;
    }

    // ----- queries -----------------------------------------------------------

    fn run_query(&mut self, mode: RunMode, ctx: &egui::Context) {
        if self.running.is_some() {
            return;
        }
        let Some(db) = &self.db else {
            self.err("Open a database before running queries.");
            return;
        };
        let params: BTreeMap<String, Value> = match self.params.trim() {
            "" => BTreeMap::new(),
            text => match serde_json::from_str::<Value>(text) {
                Ok(Value::Map(m)) => m,
                Ok(_) => {
                    self.query_error = Some("Parameters must be a JSON object, e.g. {\"id\": \"web-01\"}".into());
                    return;
                }
                Err(e) => {
                    self.query_error = Some(format!("Parameters are not valid JSON: {e}"));
                    return;
                }
            },
        };
        let text = match mode {
            RunMode::Run => self.query.clone(),
            RunMode::Explain => format!("EXPLAIN {}", self.query),
            RunMode::Profile => format!("PROFILE {}", self.query),
        };
        self.settings.remember_query(&self.query);
        let cancel = CancellationToken::new();
        let mut opts = QueryOptions::new();
        opts.parameters = params;
        opts.timeout = (self.timeout_secs > 0).then(|| Duration::from_secs(u64::from(self.timeout_secs)));
        opts.read_only = self.query_read_only || db.read_only;
        opts.cancellation = Some(cancel.clone());
        let graph = Arc::clone(&db.graph);
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = lock(&graph).query(&text, &opts);
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.query_error = None;
        self.running = Some(Running { rx, cancel, started: Instant::now(), mode });
    }

    fn poll_query(&mut self) {
        let Some(running) = &self.running else { return };
        let Ok(outcome) = running.rx.try_recv() else { return };
        let mode = running.mode;
        self.running = None;
        match outcome {
            Ok(r) => {
                let s = &r.stats;
                let total = s.parse_time + s.plan_time + s.execution_time;
                let mut msg = format!("{} row(s) in {:.1} ms", r.rows.len(), total.as_secs_f64() * 1000.0);
                if s.contains_updates() {
                    msg.push_str(&format!(
                        " · created {} node(s), {} edge(s); deleted {} node(s), {} edge(s); {} propert(ies) set",
                        s.nodes_created, s.edges_created, s.nodes_deleted, s.edges_deleted, s.properties_set
                    ));
                }
                self.ok(msg);
                let updated = s.contains_updates();
                self.result = Some(r);
                self.result_mode = mode;
                if updated {
                    self.refresh_stats();
                    self.refresh_view_entities();
                    self.data.loaded = false;
                }
            }
            Err(e) => {
                self.query_error = Some(e.to_string());
                self.err("The query failed. See the Query tab for details.");
            }
        }
    }

    /// Re-read shown entities after a write: drop deleted ones, refresh the rest.
    fn refresh_view_entities(&mut self) {
        let ids = self.view.node_ids();
        let fresh = self.with_graph(|g| Ok(ids.iter().map(|id| (id.clone(), g.get_node(id))).collect::<Vec<_>>()));
        for (id, node) in fresh.unwrap_or_default() {
            match node {
                Some(n) => self.view.add_node(n, None),
                None => self.view.remove_node(&id),
            }
        }
        let shown: Vec<Node> = self.view.node_ids().iter().filter_map(|id| self.view_node(id)).collect();
        self.show_nodes(shown, None);
        self.inspector.for_sel = None;
    }

    fn view_node(&self, id: &str) -> Option<Node> {
        self.db.as_ref().and_then(|d| d.graph.try_lock().ok().and_then(|g| g.get_node(id)))
    }

    // ----- CRUD --------------------------------------------------------------

    fn create_node(&mut self) {
        let Some(form) = &self.node_form else { return };
        let props = match form.props.parse() {
            Ok(p) => p,
            Err(e) => return self.err(e),
        };
        let mut id = form.id.trim().to_string();
        let labels: Vec<String> =
            form.labels.split([',', ':', ' ']).map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
        let created = self.with_graph(|g| {
            if id.is_empty() {
                id = fresh_id(g, "n", true);
            }
            let mut node = Node::new(id.clone());
            node.labels = labels.into_iter().collect();
            node.properties = props;
            g.add_node(node)?;
            Ok(g.get_node(&id))
        });
        if let Some(Some(node)) = created {
            self.node_form = None;
            let id = node.id.clone();
            self.show_nodes(vec![node], None);
            self.view.selected = Some(Selection::Node(id.clone()));
            self.tab = Tab::Graph;
            self.refresh_stats();
            self.data.loaded = false;
            self.ok(format!("Created node {id}"));
        }
    }

    fn create_edge(&mut self) {
        let Some(form) = &self.edge_form else { return };
        let props = match form.props.parse() {
            Ok(p) => p,
            Err(e) => return self.err(e),
        };
        let (source, target, ty) =
            (form.source.trim().to_string(), form.target.trim().to_string(), form.edge_type.trim().to_string());
        if source.is_empty() || target.is_empty() || ty.is_empty() {
            return self.err("Source, target, and type are required.");
        }
        let (mut id, directed) = (form.id.trim().to_string(), form.directed);
        let created = self.with_graph(|g| {
            if id.is_empty() {
                id = fresh_id(g, "e", false);
            }
            let mut edge = Edge::new(id.clone(), source.clone(), target.clone(), ty.clone());
            edge.directed = directed;
            edge.properties = props;
            g.add_edge(edge)?;
            let nodes: Vec<Node> = [&source, &target].iter().filter_map(|n| g.get_node(n)).collect();
            Ok((g.get_edge(&id), nodes))
        });
        if let Some((Some(edge), nodes)) = created {
            self.edge_form = None;
            let id = edge.id.clone();
            self.show_nodes(nodes, None);
            self.view.add_edge(edge);
            self.view.selected = Some(Selection::Edge(id.clone()));
            self.tab = Tab::Graph;
            self.refresh_stats();
            self.data.loaded = false;
            self.ok(format!("Created relationship {id}"));
        }
    }

    fn delete_node(&mut self, id: &str) {
        let id = id.to_string();
        if let Some(n) = self.with_graph(|g| g.delete_node(&id, true)) {
            self.view.remove_node(&id);
            self.refresh_stats();
            self.data.loaded = false;
            self.ok(format!("Deleted node {id} and {n} relationship(s)."));
        }
    }

    fn delete_edge(&mut self, id: &str) {
        let id = id.to_string();
        if self.with_graph(|g| g.delete_edge(&id)).is_some() {
            self.view.remove_edge(&id);
            self.refresh_stats();
            self.data.loaded = false;
            self.ok(format!("Deleted relationship {id}."));
        }
    }

    fn apply_inspector(&mut self) {
        let props = match self.inspector.props.parse() {
            Ok(p) => p,
            Err(e) => return self.err(e),
        };
        let Some(sel) = self.inspector.for_sel.clone() else { return };
        let done = match &sel {
            Selection::Node(id) => {
                let id = id.clone();
                self.with_graph(|g| {
                    let mut update = props.clone();
                    if let Some(old) = g.get_node(&id) {
                        for k in old.properties.keys().filter(|k| !props.contains_key(*k)) {
                            update.insert(k.clone(), Value::Null);
                        }
                    }
                    g.update_node(&id, update)?;
                    Ok(g.get_node(&id).map(|n| vec![n]).unwrap_or_default())
                })
                .map(|nodes| self.show_nodes(nodes, None))
            }
            Selection::Edge(id) => {
                let id = id.clone();
                self.with_graph(|g| {
                    let mut update = props.clone();
                    if let Some(old) = g.get_edge(&id) {
                        for k in old.properties.keys().filter(|k| !props.contains_key(*k)) {
                            update.insert(k.clone(), Value::Null);
                        }
                    }
                    g.update_edge(&id, update)?;
                    Ok(g.get_edge(&id))
                })
                .map(|e| {
                    if let Some(e) = e {
                        self.view.add_edge(e);
                    }
                })
            }
        };
        if done.is_some() {
            self.inspector.for_sel = None;
            self.data.loaded = false;
            self.ok("Changes applied.");
        }
    }

    fn change_label(&mut self, id: &str, label: &str, add: bool) {
        let (id, label) = (id.to_string(), label.trim().to_string());
        if label.is_empty() {
            return;
        }
        let node = self.with_graph(|g| {
            if add {
                g.add_label(&id, &label)?
            } else {
                g.remove_label(&id, &label)?
            }
            Ok(g.get_node(&id))
        });
        if let Some(Some(n)) = node {
            self.show_nodes(vec![n], None);
            self.inspector.new_label.clear();
            self.refresh_stats();
            self.data.loaded = false;
        }
    }

    fn transaction(&mut self, action: &str) {
        let r = self.with_graph(|g| match action {
            "begin" => g.begin(),
            "commit" => g.commit(),
            _ => g.rollback(),
        });
        if r.is_some() {
            match action {
                "begin" => self.ok("Transaction started. Changes stay private until you commit."),
                "commit" => self.ok("Transaction committed."),
                _ => {
                    self.ok("Transaction rolled back.");
                    self.refresh_view_entities();
                }
            }
            self.refresh_stats();
            self.data.loaded = false;
        }
    }

    fn create_index(&mut self) {
        let property = self.index_form.property.trim().to_string();
        if property.is_empty() {
            return self.err("Enter the property to index.");
        }
        let def = IndexDef::new(self.index_form.kind, self.index_form.target, property);
        let name = def.to_string();
        if self.with_graph(|g| g.create_index(def)).is_some() {
            self.index_form.property.clear();
            self.refresh_stats();
            self.ok(format!("Created index {name}"));
        }
    }

    fn drop_index(&mut self, def: IndexDef) {
        let name = def.to_string();
        if self.with_graph(|g| g.drop_index(def)).is_some() {
            self.refresh_stats();
            self.ok(format!("Dropped index {name}"));
        }
    }

    fn load_data_page(&mut self) {
        let (kind, label, search, skip) =
            (self.data.kind, self.data.label.clone(), self.data.search.trim().to_lowercase(), self.data.page * PAGE);
        let matches = |id: &str, props: &Properties| {
            search.is_empty()
                || id.to_lowercase().contains(&search)
                || props.values().any(|v| v.as_str().is_some_and(|s| s.to_lowercase().contains(&search)))
        };
        let loaded = self.with_graph(|g| {
            Ok(match kind {
                DataKind::Nodes => (
                    g.nodes()
                        .filter(|n| label.is_empty() || n.labels.contains(&label))
                        .filter(|n| matches(&n.id, &n.properties))
                        .skip(skip)
                        .take(PAGE)
                        .collect(),
                    Vec::new(),
                ),
                DataKind::Edges => (
                    Vec::new(),
                    g.edges()
                        .filter(|e| label.is_empty() || e.edge_type == label)
                        .filter(|e| matches(&e.id, &e.properties))
                        .skip(skip)
                        .take(PAGE)
                        .collect(),
                ),
            })
        });
        if let Some((nodes, edges)) = loaded {
            self.data.nodes = nodes;
            self.data.edges = edges;
        }
        self.data.loaded = true;
    }
}

fn with_graph_ext(p: PathBuf) -> PathBuf {
    if p.extension().is_some() { p } else { p.with_extension("graph") }
}

/// `<prefix>.nodes.csv` and `<prefix>.edges.csv`, accepting `x`, `x.csv` or `x.nodes.csv` as the prefix.
fn csv_paths(chosen: &Path) -> (PathBuf, PathBuf) {
    let name = chosen.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "graph".into());
    let prefix = name.trim_end_matches(".csv").trim_end_matches(".nodes").trim_end_matches(".edges").to_string();
    (chosen.with_file_name(format!("{prefix}.nodes.csv")), chosen.with_file_name(format!("{prefix}.edges.csv")))
}

fn journal_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".journal");
    PathBuf::from(s)
}

fn fresh_id(g: &Graph, prefix: &str, node: bool) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let mut n = nanos as u64;
    loop {
        let id = format!("{prefix}-{:x}", n & 0xffff_ffff_ffff);
        let taken = if node { g.contains_node(&id) } else { g.contains_edge(&id) };
        if !taken {
            return id;
        }
        n = n.wrapping_add(1);
    }
}

fn cell_text(v: &ResultValue) -> String {
    match v {
        ResultValue::Null => "null".into(),
        ResultValue::Bool(b) => b.to_string(),
        ResultValue::Int(i) => i.to_string(),
        ResultValue::Float(f) => f.to_string(),
        ResultValue::String(s) => s.clone(),
        ResultValue::List(items) => format!("[{}]", items.iter().map(cell_text).collect::<Vec<_>>().join(", ")),
        ResultValue::Map(m) => {
            format!("{{{}}}", m.iter().map(|(k, v)| format!("{k}: {}", cell_text(v))).collect::<Vec<_>>().join(", "))
        }
        ResultValue::Node(n) => format!("({}{})", n.id, n.labels.iter().map(|l| format!(":{l}")).collect::<String>()),
        ResultValue::Edge(e) => format!("[{}:{} {}→{}]", e.id, e.edge_type, e.source, e.target),
        ResultValue::Path(p) => {
            let ids: Vec<&str> = p.nodes.iter().map(|n| n.id.as_str()).collect();
            format!("path({} hops): {}", p.len(), ids.join(" → "))
        }
    }
}

/// A small filled circle in a label's color (font-independent).
fn color_chip(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}

fn props_text(p: &Properties) -> String {
    p.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", ")
}

// ----- UI ------------------------------------------------------------------

impl eframe::App for StudioApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_query();
        if self.running.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        self.shortcuts(ctx);

        egui::TopBottomPanel::top("menu").show(ctx, |ui| self.menu_bar(ui));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui));
        egui::SidePanel::left("explorer").resizable(true).default_width(250.0).min_width(200.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.explorer(ui));
        });
        egui::SidePanel::right("inspector").resizable(true).default_width(320.0).min_width(240.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.inspector_ui(ui));
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Graph, RichText::new("Graph").size(14.0));
                ui.selectable_value(&mut self.tab, Tab::Query, RichText::new("Query").size(14.0));
                ui.selectable_value(&mut self.tab, Tab::Data, RichText::new("Data").size(14.0));
            });
            ui.separator();
            match self.tab {
                Tab::Graph => self.graph_tab(ui),
                Tab::Query => self.query_tab(ui),
                Tab::Data => self.data_tab(ui),
            }
        });
        self.dialogs(ctx);
    }
}

impl Drop for StudioApp {
    fn drop(&mut self) {
        self.close_db();
    }
}

impl StudioApp {
    fn shortcuts(&mut self, ctx: &egui::Context) {
        let (run, save, open, nn, ne) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&RUN),
                i.consume_shortcut(&SAVE),
                i.consume_shortcut(&OPEN),
                i.consume_shortcut(&NEW_NODE),
                i.consume_shortcut(&NEW_EDGE),
            )
        });
        if run {
            self.tab = Tab::Query;
            self.run_query(RunMode::Run, ctx);
        }
        if save {
            self.save();
        }
        if open {
            self.open_dialog(false);
        }
        if nn && self.writable() {
            self.node_form = Some(NodeForm::default());
        }
        if ne && self.writable() {
            self.edge_form = Some(self.edge_form_from_selection());
        }
    }

    fn writable(&self) -> bool {
        self.db.as_ref().is_some_and(|d| !d.read_only)
    }

    fn in_transaction(&self) -> bool {
        self.db.as_ref().is_some_and(|d| d.graph.try_lock().is_ok_and(|g| g.in_transaction()))
    }

    fn edge_form_from_selection(&self) -> EdgeForm {
        EdgeForm { source: self.view.selected_node().map(|n| n.id.clone()).unwrap_or_default(), ..EdgeForm::default() }
    }

    fn menu_bar(&mut self, ui: &mut Ui) {
        let has_db = self.db.is_some();
        let writable = self.writable();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New database…").clicked() {
                    self.new_db(false);
                    ui.close();
                }
                if ui.button("New sample database…").clicked() {
                    self.new_db(true);
                    ui.close();
                }
                ui.separator();
                if ui.add(egui::Button::new("Open…").shortcut_text("Ctrl+O")).clicked() {
                    self.open_dialog(false);
                    ui.close();
                }
                if ui.button("Open read-only…").clicked() {
                    self.open_dialog(true);
                    ui.close();
                }
                ui.menu_button("Open recent", |ui| {
                    if self.settings.recent.is_empty() {
                        ui.label(RichText::new("No recent files").weak());
                    }
                    for p in self.settings.recent.clone() {
                        if ui.button(p.display().to_string()).clicked() && self.can_switch_db() {
                            self.open_path(&p, false);
                            ui.close();
                        }
                    }
                });
                ui.separator();
                if ui.add_enabled(writable, egui::Button::new("Save").shortcut_text("Ctrl+S")).clicked() {
                    self.save();
                    ui.close();
                }
                if ui.add_enabled(has_db, egui::Button::new("Save a copy as…")).clicked() {
                    self.save_as();
                    ui.close();
                }
                if ui.add_enabled(has_db, egui::Button::new("Validate file")).clicked() {
                    self.validate();
                    ui.close();
                }
                ui.separator();
                ui.add_enabled_ui(writable, |ui| {
                    ui.menu_button("Import", |ui| {
                        for (label, fmt) in [("JSON…", "json"), ("JSON Lines…", "jsonl"), ("CSV…", "csv")] {
                            if ui.button(label).clicked() {
                                self.import(fmt);
                                ui.close();
                            }
                        }
                    });
                });
                ui.add_enabled_ui(has_db, |ui| {
                    ui.menu_button("Export", |ui| {
                        for (label, fmt) in
                            [("JSON…", "json"), ("JSON Lines…", "jsonl"), ("GraphML…", "graphml"), ("CSV…", "csv")]
                        {
                            if ui.button(label).clicked() {
                                self.export(fmt);
                                ui.close();
                            }
                        }
                    });
                });
                ui.separator();
                if ui.add_enabled(has_db, egui::Button::new("Close database")).clicked() {
                    self.request_close();
                    ui.close();
                }
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui.add_enabled(writable, egui::Button::new("New node…").shortcut_text("Ctrl+N")).clicked() {
                    self.node_form = Some(NodeForm::default());
                    ui.close();
                }
                if ui.add_enabled(writable, egui::Button::new("New relationship…").shortcut_text("Ctrl+E")).clicked()
                {
                    self.edge_form = Some(self.edge_form_from_selection());
                    ui.close();
                }
                ui.separator();
                let in_tx = self.in_transaction();
                if ui.add_enabled(writable && !in_tx, egui::Button::new("Begin transaction")).clicked() {
                    self.transaction("begin");
                    ui.close();
                }
                if ui.add_enabled(in_tx, egui::Button::new("Commit transaction")).clicked() {
                    self.transaction("commit");
                    ui.close();
                }
                if ui.add_enabled(in_tx, egui::Button::new("Roll back transaction")).clicked() {
                    self.transaction("rollback");
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                let mut dark = self.settings.dark;
                if ui.checkbox(&mut dark, "Dark theme").changed() {
                    self.settings.dark = dark;
                    theme::apply(ui.ctx(), dark);
                    self.settings.save();
                }
                ui.checkbox(&mut self.view.show_edge_labels, "Relationship labels");
                ui.separator();
                if ui.add_enabled(has_db, egui::Button::new("Show overview")).clicked() {
                    self.load_overview();
                    self.tab = Tab::Graph;
                    ui.close();
                }
                if ui.button("Clear graph view").clicked() {
                    self.view.clear();
                    ui.close();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Query language guide").clicked() {
                    self.show_help = true;
                    ui.close();
                }
                if ui.button("About GrapHydra Studio").clicked() {
                    self.show_about = true;
                    ui.close();
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let pal = self.pal();
        ui.horizontal(|ui| {
            match &self.db {
                Some(db) => {
                    ui.label(RichText::new(db.path.display().to_string()).small());
                    if db.read_only {
                        ui.label(
                            RichText::new(" READ-ONLY ")
                                .small()
                                .strong()
                                .color(pal.surface)
                                .background_color(pal.muted),
                        );
                    }
                }
                None => {
                    ui.label(RichText::new("No database").small().weak());
                }
            }
            if self.in_transaction() {
                ui.label(RichText::new("TRANSACTION OPEN").small().color(pal.series[3]));
            }
            ui.separator();
            let color = match self.status.kind {
                StatusKind::Info => pal.ink2,
                StatusKind::Success => pal.success,
                StatusKind::Error => pal.danger,
            };
            ui.label(RichText::new(&self.status.text).small().color(color));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(s) = &self.stats {
                    ui.label(
                        RichText::new(format!(
                            "{} nodes · {} relationships · generation {}",
                            s.nodes, s.edges, s.generation
                        ))
                        .small(),
                    );
                }
            });
        });
    }

    fn explorer(&mut self, ui: &mut Ui) {
        let pal = self.pal();
        ui.add_space(4.0);
        ui.heading("Explorer");
        ui.add_space(4.0);
        let Some(stats) = self.stats.clone() else {
            ui.label("No database is open.");
            ui.add_space(8.0);
            if ui.button("Create database…").clicked() {
                self.new_db(false);
            }
            if ui.button("Create sample database…").clicked() {
                self.new_db(true);
            }
            if ui.button("Open database…").clicked() {
                self.open_dialog(false);
            }
            return;
        };
        egui::CollapsingHeader::new("Database").default_open(true).show(ui, |ui| {
            egui::Grid::new("dbinfo").num_columns(2).show(ui, |ui| {
                ui.label(RichText::new("Nodes").weak());
                ui.label(stats.nodes.to_string());
                ui.end_row();
                ui.label(RichText::new("Relationships").weak());
                ui.label(stats.edges.to_string());
                ui.end_row();
                ui.label(RichText::new("Generation").weak());
                ui.label(stats.generation.to_string());
                ui.end_row();
                ui.label(RichText::new("Journal").weak());
                ui.label(format!("{:.1} KiB", stats.journal_bytes as f64 / 1024.0));
                ui.end_row();
                ui.label(RichText::new("Graph id").weak());
                ui.label(RichText::new(&stats.graph_id).small()).on_hover_text(&stats.graph_id);
                ui.end_row();
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(self.writable(), egui::Button::new("Save"))
                    .on_hover_text("Checkpoint (Ctrl+S)")
                    .clicked()
                {
                    self.save();
                }
                if ui.button("Validate").clicked() {
                    self.validate();
                }
                if ui.button("Refresh").clicked() {
                    if self.db.as_ref().is_some_and(|d| d.read_only) {
                        let _ = self.with_graph(Graph::refresh);
                    }
                    self.refresh_stats();
                    self.data.loaded = false;
                }
            });
        });
        egui::CollapsingHeader::new(format!("Labels ({})", stats.labels.len())).default_open(true).show(ui, |ui| {
            if stats.labels.is_empty() {
                ui.label(RichText::new("No labels yet").weak());
            }
            for (label, count) in &stats.labels {
                let c = pal.label_color(self.label_slots.get(label));
                ui.horizontal(|ui| {
                    color_chip(ui, c);
                    if ui.link(format!(":{label}")).on_hover_text("Show these nodes in the graph").clicked() {
                        self.show_label(label);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(count.to_string()).weak());
                    });
                });
            }
        });
        egui::CollapsingHeader::new(format!("Relationship types ({})", stats.edge_types.len()))
            .default_open(true)
            .show(ui, |ui| {
                if stats.edge_types.is_empty() {
                    ui.label(RichText::new("No relationships yet").weak());
                }
                for (ty, count) in &stats.edge_types {
                    ui.horizontal(|ui| {
                        if ui.link(format!(":{ty}")).on_hover_text("Show these relationships in the graph").clicked() {
                            self.show_edge_type(ty);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(count.to_string()).weak());
                        });
                    });
                }
            });
        egui::CollapsingHeader::new(format!("Indexes ({})", stats.indexes.len())).default_open(false).show(ui, |ui| {
            for info in &stats.indexes {
                ui.horizontal(|ui| {
                    ui.label(format!("{} {}.{}", info.def.kind, info.def.target, info.def.property))
                        .on_hover_text(format!("{} entries, {} distinct keys", info.entries, info.distinct_keys));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(self.writable(), egui::Button::new("×").small())
                            .on_hover_text("Drop index")
                            .clicked()
                        {
                            self.drop_index(info.def.clone());
                        }
                    });
                });
            }
            ui.separator();
            ui.label(RichText::new("New index").strong());
            ui.add(TextEdit::singleline(&mut self.index_form.property).hint_text("property, e.g. hostname"));
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("idx-kind").selected_text(self.index_form.kind.to_string()).show_ui(
                    ui,
                    |ui| {
                        for k in [IndexKind::Hash, IndexKind::Sorted, IndexKind::Text] {
                            ui.selectable_value(&mut self.index_form.kind, k, k.to_string());
                        }
                    },
                );
                egui::ComboBox::from_id_salt("idx-target").selected_text(self.index_form.target.to_string()).show_ui(
                    ui,
                    |ui| {
                        ui.selectable_value(&mut self.index_form.target, IndexTarget::Node, "node");
                        ui.selectable_value(&mut self.index_form.target, IndexTarget::Edge, "edge");
                    },
                );
                if ui.add_enabled(self.writable(), egui::Button::new("Create")).clicked() {
                    self.create_index();
                }
            });
            ui.label(
                RichText::new("hash: equality · sorted: ranges and prefixes · text: full-text search").small().weak(),
            );
        });
        if !stats.recovered_torn_write && stats.recovered_transactions == 0 {
            return;
        }
        ui.separator();
        ui.label(
            RichText::new(format!(
                "Recovered {} transaction(s) on open{}.",
                stats.recovered_transactions,
                if stats.recovered_torn_write { "; an incomplete trailing write was discarded" } else { "" }
            ))
            .small()
            .color(pal.series[3]),
        );
    }

    fn graph_tab(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let fit = ui.button("Fit").on_hover_text("Fit the graph to the window");
            if ui.button("Layout").on_hover_text("Re-run the automatic layout").clicked() {
                self.view.relayout();
                self.pending_fit = true;
            }
            if ui
                .add_enabled(self.db.is_some(), egui::Button::new("Overview"))
                .on_hover_text(format!("Show the first {LOAD_LIMIT} nodes"))
                .clicked()
            {
                self.load_overview();
            }
            if ui.button("Clear").clicked() {
                self.view.clear();
            }
            ui.separator();
            if ui.add_enabled(self.writable(), egui::Button::new("+ Node")).clicked() {
                self.node_form = Some(NodeForm::default());
            }
            if ui.add_enabled(self.writable(), egui::Button::new("+ Relationship")).clicked() {
                self.edge_form = Some(self.edge_form_from_selection());
            }
            ui.separator();
            ui.label(
                RichText::new(format!(
                    "{} nodes · {} relationships shown",
                    self.view.node_count(),
                    self.view.edge_count()
                ))
                .weak(),
            );
            if fit.clicked() {
                self.pending_fit = true;
            }
        });
        let pal = self.pal();
        if self.pending_fit {
            self.view.request_fit();
            self.pending_fit = false;
        }
        let actions = self.view.ui(ui, &pal, &self.label_slots);
        for a in actions {
            match a {
                CanvasAction::Select => self.inspector.for_sel = None,
                CanvasAction::Expand(id) => self.expand(&id),
                CanvasAction::DeleteNode(id) => {
                    if self.writable() {
                        self.confirm = Some(Confirm::DeleteNode(id));
                    }
                }
                CanvasAction::ConnectFrom(id) => {
                    if self.writable() {
                        self.edge_form = Some(EdgeForm { source: id, ..EdgeForm::default() });
                    }
                }
            }
        }
    }

    fn query_tab(&mut self, ui: &mut Ui) {
        let pal = self.pal();
        let running = self.running.is_some();
        ui.horizontal(|ui| {
            let run = ui
                .add_enabled(!running && self.db.is_some(), egui::Button::new(RichText::new("▶ Run").strong()))
                .on_hover_text("Ctrl+Enter");
            if run.clicked() {
                self.run_query(RunMode::Run, ui.ctx());
            }
            if ui
                .add_enabled(!running && self.db.is_some(), egui::Button::new("Explain"))
                .on_hover_text("Show the plan without running")
                .clicked()
            {
                self.run_query(RunMode::Explain, ui.ctx());
            }
            if ui
                .add_enabled(!running && self.db.is_some(), egui::Button::new("Profile"))
                .on_hover_text("Run and show the plan with counts")
                .clicked()
            {
                self.run_query(RunMode::Profile, ui.ctx());
            }
            if let Some(r) = &self.running {
                if ui.button(RichText::new("■ Cancel").color(pal.danger)).clicked() {
                    r.cancel.cancel();
                }
                ui.spinner();
                ui.label(format!("{:.1}s", r.started.elapsed().as_secs_f32()));
            }
            ui.separator();
            ui.label("Timeout");
            ui.add(egui::DragValue::new(&mut self.timeout_secs).range(0..=3600).suffix(" s"))
                .on_hover_text("0 means no timeout");
            ui.checkbox(&mut self.query_read_only, "Read-only")
                .on_hover_text("Reject any statement that would change data");
            ui.separator();
            egui::ComboBox::from_id_salt("samples").selected_text("Examples").show_ui(ui, |ui| {
                for (name, q, params) in SAMPLES {
                    if ui.selectable_label(false, *name).clicked() {
                        self.query = (*q).to_string();
                        self.params = (*params).to_string();
                    }
                }
            });
            egui::ComboBox::from_id_salt("history").selected_text("History").width(90.0).show_ui(ui, |ui| {
                if self.settings.history.is_empty() {
                    ui.label(RichText::new("Nothing yet").weak());
                }
                for q in self.settings.history.clone() {
                    let short: String = q.chars().take(80).collect();
                    if ui.selectable_label(false, short).on_hover_text(&q).clicked() {
                        self.query = q;
                    }
                }
            });
        });
        ui.add_space(4.0);
        ui.columns(2, |cols| {
            cols[0].label(RichText::new("GHQL").small().weak());
            cols[0].add(
                TextEdit::multiline(&mut self.query)
                    .code_editor()
                    .desired_rows(7)
                    .desired_width(f32::INFINITY)
                    .hint_text("MATCH (n:Host) WHERE n.os = \"linux\" RETURN n"),
            );
            cols[1].label(RichText::new("Parameters (JSON object, use as $name)").small().weak());
            cols[1]
                .add(TextEdit::multiline(&mut self.params).code_editor().desired_rows(7).desired_width(f32::INFINITY));
        });
        ui.add_space(4.0);
        if let Some(e) = &self.query_error {
            egui::Frame::new().fill(pal.danger.gamma_multiply(0.15)).corner_radius(6).inner_margin(8).show(ui, |ui| {
                ui.label(RichText::new(e).color(pal.danger).monospace());
            });
            return;
        }
        let Some(result) = &self.result else {
            ui.label(RichText::new("Results appear here. Press Ctrl+Enter to run.").weak());
            return;
        };
        if let Some(plan) = &result.plan {
            egui::CollapsingHeader::new("Query plan").default_open(self.result_mode != RunMode::Run).show(ui, |ui| {
                ui.label(RichText::new(plan.to_text()).monospace());
            });
        }
        let s = &result.stats;
        let mut draw = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} row(s)", result.rows.len())).strong());
            ui.label(
                RichText::new(format!(
                    "parse {:.2} ms · plan {:.2} ms · execute {:.2} ms · scanned {} · traversed {} · index lookups {}{}",
                    s.parse_time.as_secs_f64() * 1e3,
                    s.plan_time.as_secs_f64() * 1e3,
                    s.execution_time.as_secs_f64() * 1e3,
                    s.nodes_scanned,
                    s.edges_traversed,
                    s.index_lookups,
                    if s.statement_cache_hit { " · cached plan" } else { "" }
                ))
                .small()
                .weak(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                draw = ui.button("Show in graph").clicked();
                if ui.button("Copy as JSON").clicked() {
                    let rows: Vec<serde_json::Value> = result
                        .rows
                        .iter()
                        .map(|r| {
                            serde_json::Value::Object(
                                result.columns.iter().zip(r).map(|(c, v)| (c.clone(), serde_json::json!(cell_text(v)))).collect(),
                            )
                        })
                        .collect();
                    ui.ctx().copy_text(serde_json::to_string_pretty(&rows).unwrap_or_default());
                }
            });
        });
        if result.columns.is_empty() {
            ui.label(RichText::new("The statement returned no columns.").weak());
        } else {
            let columns = result.columns.clone();
            let rows: Vec<Vec<String>> = result.rows.iter().map(|r| r.iter().map(cell_text).collect()).collect();
            ui.push_id("results", |ui| {
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .auto_shrink(false)
                    .columns(Column::initial(200.0).at_least(60.0).clip(true), columns.len().saturating_sub(1))
                    .column(Column::remainder().at_least(60.0).clip(true))
                    .header(22.0, |mut h| {
                        for c in &columns {
                            h.col(|ui| {
                                ui.strong(c);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(20.0, rows.len(), |mut row| {
                            let i = row.index();
                            for cell in &rows[i] {
                                row.col(|ui| {
                                    ui.add(egui::Label::new(cell).truncate()).on_hover_text(cell);
                                });
                            }
                        });
                    });
            });
        }
        if draw {
            self.show_result_in_graph();
        }
    }

    fn data_tab(&mut self, ui: &mut Ui) {
        if self.db.is_none() {
            ui.label(RichText::new("Open a database to browse its data.").weak());
            return;
        }
        let mut reload = !self.data.loaded;
        let options: Vec<String> = match (&self.stats, self.data.kind) {
            (Some(s), DataKind::Nodes) => s.labels.iter().map(|(l, _)| l.clone()).collect(),
            (Some(s), DataKind::Edges) => s.edge_types.iter().map(|(t, _)| t.clone()).collect(),
            _ => Vec::new(),
        };
        ui.horizontal(|ui| {
            if ui.selectable_value(&mut self.data.kind, DataKind::Nodes, "Nodes").clicked()
                | ui.selectable_value(&mut self.data.kind, DataKind::Edges, "Relationships").clicked()
            {
                self.data.label.clear();
                self.data.page = 0;
                reload = true;
            }
            ui.separator();
            let shown = if self.data.label.is_empty() { "All".to_string() } else { format!(":{}", self.data.label) };
            egui::ComboBox::from_id_salt("data-filter").selected_text(shown).show_ui(ui, |ui| {
                if ui.selectable_label(self.data.label.is_empty(), "All").clicked() {
                    self.data.label.clear();
                    self.data.page = 0;
                    reload = true;
                }
                for o in &options {
                    if ui.selectable_label(&self.data.label == o, format!(":{o}")).clicked() {
                        self.data.label = o.clone();
                        self.data.page = 0;
                        reload = true;
                    }
                }
            });
            let search = ui
                .add(TextEdit::singleline(&mut self.data.search).hint_text("search id or text…").desired_width(200.0));
            if search.changed() {
                self.data.page = 0;
                reload = true;
            }
            ui.separator();
            if ui.add_enabled(self.data.page > 0, egui::Button::new("◀")).clicked() {
                self.data.page -= 1;
                reload = true;
            }
            ui.label(format!("page {}", self.data.page + 1));
            let full = match self.data.kind {
                DataKind::Nodes => self.data.nodes.len() == PAGE,
                DataKind::Edges => self.data.edges.len() == PAGE,
            };
            if ui.add_enabled(full, egui::Button::new("▶")).clicked() {
                self.data.page += 1;
                reload = true;
            }
        });
        if reload {
            self.load_data_page();
        }
        ui.separator();
        let mut inspect: Option<Selection> = None;
        let mut graph_nodes: Option<Vec<String>> = None;
        ui.push_id(("data", self.data.kind == DataKind::Nodes), |ui| {
            let table = TableBuilder::new(ui).striped(true).resizable(true).auto_shrink(false);
            match self.data.kind {
                DataKind::Nodes => {
                    let nodes = &self.data.nodes;
                    table
                        .column(Column::initial(160.0).at_least(80.0).clip(true))
                        .column(Column::initial(160.0).at_least(60.0).clip(true))
                        .column(Column::remainder().at_least(100.0).clip(true))
                        .column(Column::exact(150.0))
                        .header(22.0, |mut h| {
                            for c in ["id", "labels", "properties", ""] {
                                h.col(|ui| {
                                    ui.strong(c);
                                });
                            }
                        })
                        .body(|body| {
                            body.rows(22.0, nodes.len(), |mut row| {
                                let n = &nodes[row.index()];
                                row.col(|ui| {
                                    ui.add(egui::Label::new(&n.id).truncate());
                                });
                                row.col(|ui| {
                                    ui.label(n.labels.iter().map(|l| format!(":{l}")).collect::<String>());
                                });
                                let p = props_text(&n.properties);
                                row.col(|ui| {
                                    ui.add(egui::Label::new(&p).truncate()).on_hover_text(&p);
                                });
                                row.col(|ui| {
                                    ui.horizontal(|ui| {
                                        if ui.small_button("Inspect").clicked() {
                                            inspect = Some(Selection::Node(n.id.clone()));
                                        }
                                        if ui.small_button("Graph").clicked() {
                                            graph_nodes = Some(vec![n.id.clone()]);
                                        }
                                    });
                                });
                            });
                        });
                }
                DataKind::Edges => {
                    let edges = &self.data.edges;
                    table
                        .column(Column::initial(130.0).at_least(60.0).clip(true))
                        .column(Column::initial(130.0).at_least(60.0).clip(true))
                        .column(Column::initial(240.0).at_least(80.0).clip(true))
                        .column(Column::remainder().at_least(100.0).clip(true))
                        .column(Column::exact(150.0))
                        .header(22.0, |mut h| {
                            for c in ["id", "type", "source → target", "properties", ""] {
                                h.col(|ui| {
                                    ui.strong(c);
                                });
                            }
                        })
                        .body(|body| {
                            body.rows(22.0, edges.len(), |mut row| {
                                let e = &edges[row.index()];
                                row.col(|ui| {
                                    ui.add(egui::Label::new(&e.id).truncate());
                                });
                                row.col(|ui| {
                                    ui.label(format!(":{}", e.edge_type));
                                });
                                row.col(|ui| {
                                    // `--` is how GHQL writes an undirected relationship.
                                    let arrow = if e.directed { "→" } else { "--" };
                                    ui.add(egui::Label::new(format!("{} {arrow} {}", e.source, e.target)).truncate());
                                });
                                let p = props_text(&e.properties);
                                row.col(|ui| {
                                    ui.add(egui::Label::new(&p).truncate()).on_hover_text(&p);
                                });
                                row.col(|ui| {
                                    ui.horizontal(|ui| {
                                        if ui.small_button("Inspect").clicked() {
                                            inspect = Some(Selection::Edge(e.id.clone()));
                                        }
                                        if ui.small_button("Graph").clicked() {
                                            graph_nodes = Some(vec![e.source.clone(), e.target.clone()]);
                                        }
                                    });
                                });
                            });
                        });
                }
            }
        });
        if let Some(ids) = graph_nodes {
            let nodes = self.with_graph(|g| Ok(ids.iter().filter_map(|id| g.get_node(id)).collect::<Vec<_>>()));
            if let Some(nodes) = nodes {
                let first = ids.first().cloned();
                self.show_nodes(nodes, None);
                if let Some(id) = first {
                    self.view.selected = Some(Selection::Node(id));
                }
                self.pending_fit = true;
                self.tab = Tab::Graph;
            }
        }
        if let Some(sel) = inspect {
            self.inspect_outside_view(sel);
        }
    }

    /// Select an entity even if it is not drawn yet (adds it to the view).
    fn inspect_outside_view(&mut self, sel: Selection) {
        match &sel {
            Selection::Node(id) => {
                if !self.view.contains_node(id)
                    && let Some(n) = self.view_node(id)
                {
                    self.show_nodes(vec![n], None);
                }
            }
            Selection::Edge(id) => {
                let id = id.clone();
                let found = self.with_graph(|g| {
                    let e = g.get_edge(&id);
                    let nodes = e
                        .iter()
                        .flat_map(|e| [g.get_node(&e.source), g.get_node(&e.target)])
                        .flatten()
                        .collect::<Vec<_>>();
                    Ok((e, nodes))
                });
                if let Some((Some(e), nodes)) = found {
                    self.show_nodes(nodes, None);
                    self.view.add_edge(e);
                }
            }
        }
        self.view.selected = Some(sel);
        self.inspector.for_sel = None;
    }

    fn inspector_ui(&mut self, ui: &mut Ui) {
        let pal = self.pal();
        ui.add_space(4.0);
        ui.heading("Inspector");
        ui.add_space(4.0);
        let writable = self.writable();
        let sel = self.view.selected.clone();
        if self.inspector.for_sel != sel {
            self.inspector.props = match &sel {
                Some(Selection::Node(_)) => {
                    self.view.selected_node().map(|n| PropRows::from(&n.properties)).unwrap_or_default()
                }
                Some(Selection::Edge(_)) => {
                    self.view.selected_edge().map(|e| PropRows::from(&e.properties)).unwrap_or_default()
                }
                None => PropRows::default(),
            };
            self.inspector.for_sel = sel.clone();
        }
        match sel {
            None => {
                ui.label(
                    RichText::new("Click a node or relationship in the graph to see and edit its details.").weak(),
                );
            }
            Some(Selection::Node(_)) => {
                let Some(node) = self.view.selected_node().cloned() else { return };
                ui.label(RichText::new(graph_view::caption(&node)).size(17.0).strong());
                ui.label(RichText::new(format!("id: {}", node.id)).monospace().small());
                ui.add_space(6.0);
                ui.label(RichText::new("Labels").strong());
                ui.horizontal_wrapped(|ui| {
                    for l in node.labels.clone() {
                        let c = pal.label_color(self.label_slots.get(&l));
                        ui.label(RichText::new(format!(" :{l} ")).color(Color32::WHITE).background_color(c));
                        if writable && ui.small_button("×").on_hover_text("Remove label").clicked() {
                            self.change_label(&node.id, &l, false);
                        }
                    }
                    if node.labels.is_empty() {
                        ui.label(RichText::new("none").weak());
                    }
                });
                if writable {
                    ui.horizontal(|ui| {
                        let r = ui.add(
                            TextEdit::singleline(&mut self.inspector.new_label)
                                .hint_text("add label")
                                .desired_width(140.0),
                        );
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if ui.button("Add").clicked() || enter {
                            let l = self.inspector.new_label.clone();
                            self.change_label(&node.id, &l, true);
                        }
                    });
                }
                ui.add_space(6.0);
                self.properties_editor(ui, writable);
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Expand neighbors").clicked() {
                        self.expand(&node.id);
                    }
                    if writable && ui.button("Connect from here…").clicked() {
                        self.edge_form = Some(EdgeForm { source: node.id.clone(), ..EdgeForm::default() });
                    }
                    if ui.button("Query this node").clicked() {
                        self.query = "MATCH (n {id: $id})-[r]-(m) RETURN n, r, m".into();
                        self.params = serde_json::json!({ "id": node.id }).to_string();
                        self.tab = Tab::Query;
                    }
                    if writable && ui.button(RichText::new("Delete node…").color(pal.danger)).clicked() {
                        self.confirm = Some(Confirm::DeleteNode(node.id.clone()));
                    }
                });
            }
            Some(Selection::Edge(_)) => {
                let Some(edge) = self.view.selected_edge().cloned() else { return };
                ui.label(RichText::new(format!(":{}", edge.edge_type)).size(17.0).strong());
                ui.label(RichText::new(format!("id: {}", edge.id)).monospace().small());
                ui.add_space(4.0);
                egui::Grid::new("edge-ends").num_columns(2).show(ui, |ui| {
                    ui.label(RichText::new("From").weak());
                    if ui.link(&edge.source).clicked() {
                        self.view.selected = Some(Selection::Node(edge.source.clone()));
                    }
                    ui.end_row();
                    ui.label(RichText::new("To").weak());
                    if ui.link(&edge.target).clicked() {
                        self.view.selected = Some(Selection::Node(edge.target.clone()));
                    }
                    ui.end_row();
                    ui.label(RichText::new("Directed").weak());
                    ui.label(if edge.directed { "yes" } else { "no" });
                    ui.end_row();
                });
                ui.add_space(6.0);
                self.properties_editor(ui, writable);
                ui.add_space(8.0);
                if writable && ui.button(RichText::new("Delete relationship…").color(pal.danger)).clicked() {
                    self.confirm = Some(Confirm::DeleteEdge(edge.id.clone()));
                }
            }
        }
    }

    fn properties_editor(&mut self, ui: &mut Ui, writable: bool) {
        ui.label(RichText::new("Properties").strong());
        if writable {
            self.inspector.props.ui(ui, "inspector-props");
            ui.label(RichText::new("Values: text, or JSON such as 42, true, [1, 2], {\"k\": 1}").small().weak());
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Apply changes").strong()).clicked() {
                    self.apply_inspector();
                }
                if ui.button("Revert").clicked() {
                    self.inspector.for_sel = None;
                }
            });
        } else {
            egui::Grid::new("ro-props").num_columns(2).striped(true).show(ui, |ui| {
                for (k, v) in &self.inspector.props.0 {
                    ui.label(RichText::new(k).weak());
                    ui.label(v);
                    ui.end_row();
                }
            });
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let pal = self.pal();
        if self.node_form.is_some() {
            let mut open = true;
            let (mut create, mut cancel) = (false, false);
            egui::Window::new("New node").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
                let Some(f) = &mut self.node_form else { return };
                egui::Grid::new("node-form").num_columns(2).show(ui, |ui| {
                    ui.label("Id");
                    ui.add(TextEdit::singleline(&mut f.id).hint_text("leave empty to generate"));
                    ui.end_row();
                    ui.label("Labels");
                    ui.add(TextEdit::singleline(&mut f.labels).hint_text("e.g. Person, Employee"));
                    ui.end_row();
                });
                ui.add_space(4.0);
                ui.label(RichText::new("Properties").strong());
                f.props.ui(ui, "node-form-props");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    create = ui.button(RichText::new("Create node").strong()).clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
            if create {
                self.create_node();
            }
            if cancel || !open {
                self.node_form = None;
            }
        }
        if self.edge_form.is_some() {
            let mut open = true;
            let (mut create, mut cancel) = (false, false);
            let shown = self.view.node_ids();
            let types: Vec<String> =
                self.stats.iter().flat_map(|s| s.edge_types.iter().map(|(t, _)| t.clone())).collect();
            egui::Window::new("New relationship").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
                let Some(f) = &mut self.edge_form else { return };
                egui::Grid::new("edge-form").num_columns(2).show(ui, |ui| {
                    for (label, field, salt) in [("From node", &mut f.source, "src"), ("To node", &mut f.target, "dst")]
                    {
                        ui.label(label);
                        ui.horizontal(|ui| {
                            ui.add(TextEdit::singleline(field).hint_text("node id").desired_width(170.0));
                            egui::ComboBox::from_id_salt(salt).selected_text("pick").width(60.0).show_ui(ui, |ui| {
                                for id in &shown {
                                    if ui.selectable_label(field == id, id).clicked() {
                                        *field = id.clone();
                                    }
                                }
                            });
                        });
                        ui.end_row();
                    }
                    ui.label("Type");
                    ui.horizontal(|ui| {
                        ui.add(TextEdit::singleline(&mut f.edge_type).hint_text("e.g. KNOWS").desired_width(170.0));
                        egui::ComboBox::from_id_salt("types").selected_text("pick").width(60.0).show_ui(ui, |ui| {
                            for t in &types {
                                if ui.selectable_label(&f.edge_type == t, t).clicked() {
                                    f.edge_type = t.clone();
                                }
                            }
                        });
                    });
                    ui.end_row();
                    ui.label("Id");
                    ui.add(TextEdit::singleline(&mut f.id).hint_text("leave empty to generate"));
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut f.directed, "Directed");
                    ui.end_row();
                });
                ui.add_space(4.0);
                ui.label(RichText::new("Properties").strong());
                f.props.ui(ui, "edge-form-props");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    create = ui.button(RichText::new("Create relationship").strong()).clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
            if create {
                self.create_edge();
            }
            if cancel || !open {
                self.edge_form = None;
            }
        }
        if let Some(confirm) = &self.confirm {
            let (title, body, action) = match confirm {
                Confirm::DeleteNode(id) => {
                    let degree =
                        self.with_graph_ro(|g| g.neighbors(id, Direction::Both, None).map(|v| v.len()).unwrap_or(0));
                    (
                        "Delete node",
                        format!("Delete node {id} and its {degree} relationship(s)? This cannot be undone."),
                        "Delete",
                    )
                }
                Confirm::DeleteEdge(id) => {
                    ("Delete relationship", format!("Delete relationship {id}? This cannot be undone."), "Delete")
                }
                Confirm::CloseWithTransaction => (
                    "Uncommitted transaction",
                    "A transaction is open. Closing discards its changes. Commit first to keep them.".to_string(),
                    "Discard and close",
                ),
            };
            let mut choice = None;
            let modal = egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading(title);
                ui.add_space(6.0);
                ui.label(body);
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(action).color(pal.danger).strong()).clicked() {
                        choice = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(false);
                    }
                });
            });
            if modal.should_close() && choice.is_none() {
                choice = Some(false);
            }
            if let Some(yes) = choice {
                let confirm = self.confirm.take();
                if yes {
                    match confirm {
                        Some(Confirm::DeleteNode(id)) => self.delete_node(&id),
                        Some(Confirm::DeleteEdge(id)) => self.delete_edge(&id),
                        Some(Confirm::CloseWithTransaction) => {
                            let _ = self.with_graph(Graph::rollback);
                            self.close_db();
                            self.info("Database closed; the open transaction was discarded.");
                        }
                        None => {}
                    }
                }
            }
        }
        if self.show_help {
            let mut open = true;
            let mut load: Option<String> = None;
            egui::Window::new("GHQL quick guide").open(&mut open).default_width(560.0).show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(520.0).show(ui, |ui| {
                    ui.label(
                        "GrapHydra queries use a Cypher-like language. Click an example to load it into the editor.",
                    );
                    ui.add_space(6.0);

                    egui::Grid::new("guide").num_columns(2).striped(true).spacing([12.0, 8.0]).show(ui, |ui| {
                        for &(name, q) in GUIDE {
                            ui.label(RichText::new(name).strong());
                            if ui.link(RichText::new(q).monospace()).clicked() {
                                load = Some(q.to_string());
                            }
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                    ui.label(RichText::new("Tips").strong());
                    ui.label("• Ctrl+Enter runs the query; Cancel stops a long one.");
                    ui.label(
                        "• Use $name placeholders with the Parameters box instead of pasting values into the query.",
                    );
                    ui.label("• Explain shows which indexes a query uses. Profile also runs it and counts the work.");
                    ui.label("• Variable-length patterns need an upper bound, e.g. [*1..4].");
                    ui.label("• The full reference is docs/query-language.md in the GrapHydra repository.");
                });
            });
            if let Some(q) = load {
                self.query = q;
                self.params = "{}".into();
                self.tab = Tab::Query;
            }
            self.show_help = open;
        }
        if self.show_about {
            let mut open = true;
            egui::Window::new("About").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
                ui.heading("GrapHydra Studio");
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.label(format!("Engine graphydra-core {}", graphydra_core::VERSION));
                ui.add_space(6.0);
                ui.label("A desktop workbench for GrapHydra .graph databases:");
                ui.label("create, browse, visualize, query, and edit graphs.");
            });
            self.show_about = open;
        }
    }

    /// Read-only peek used while drawing dialogs; returns a default when busy.
    fn with_graph_ro<T: Default>(&self, f: impl FnOnce(&Graph) -> T) -> T {
        self.db.as_ref().and_then(|d| d.graph.try_lock().ok().map(|g| f(&g))).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_inputs_parse_as_json_or_text() {
        assert_eq!(input_to_value("42"), Ok(Value::Int(42)));
        assert_eq!(input_to_value("true"), Ok(Value::Bool(true)));
        assert_eq!(input_to_value("\"quoted\""), Ok(Value::String("quoted".into())));
        assert_eq!(input_to_value("plain text"), Ok(Value::String("plain text".into())));
        assert_eq!(input_to_value("[1, 2]"), Ok(Value::List(vec![Value::Int(1), Value::Int(2)])));
        assert!(input_to_value("{broken").is_err(), "malformed JSON must not silently become a string");
        // Round trip through the editor representation.
        for v in [Value::Int(7), Value::Float(1.5), Value::String("x".into()), Value::Bool(false)] {
            assert_eq!(input_to_value(&value_to_input(&v)), Ok(v));
        }
    }

    #[test]
    fn property_rows_require_names_and_skip_blank_rows() {
        let rows = PropRows(vec![("a".into(), "1".into()), (String::new(), String::new())]);
        assert_eq!(rows.parse().map(|p| p.len()), Ok(1));
        assert!(PropRows(vec![(String::new(), "orphan".into())]).parse().is_err());
    }

    #[test]
    fn sample_database_builds_and_queries() {
        let dir = std::env::temp_dir().join(format!("gv-studio-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.graph");
        let mut g = Graph::create(&path).unwrap();
        crate::demo::populate(&mut g).unwrap();
        assert_eq!((g.node_count(), g.edge_count()), (11, 11));
        let hosts = g.run("MATCH (h:Host) RETURN h.hostname ORDER BY h.hostname").unwrap();
        assert_eq!(hosts.rows.len(), 3);
        g.close().unwrap();
        // Reopens cleanly with its indexes.
        let g = Graph::open_read_only(&path).unwrap();
        assert_eq!(g.indexes().len(), 2);
        assert!(validate_file(&path).valid);
        drop(g);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_built_in_example_runs_on_the_sample_data() {
        let dir = std::env::temp_dir().join(format!("gv-studio-examples-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut g = Graph::create(dir.join("s.graph")).unwrap();
        crate::demo::populate(&mut g).unwrap();
        for (_, q, params) in SAMPLES {
            let Ok(Value::Map(parameters)) = serde_json::from_str::<Value>(params) else {
                panic!("bad params {params}")
            };
            let opts = QueryOptions { parameters, ..QueryOptions::new() };
            let r = g.explain(q, &opts);
            assert!(r.is_ok(), "{q}: {r:?}");
        }
        for (_, q) in GUIDE {
            // Each guide example runs for real; a transaction keeps them independent.
            g.begin().unwrap();
            let r = g.run(q);
            assert!(r.is_ok(), "{q}: {r:?}");
            g.rollback().unwrap();
        }
        drop(g);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn csv_export_names_match_the_cli() {
        for chosen in ["out", "out.csv", "out.nodes.csv", "out.edges.csv"] {
            let (n, e) = csv_paths(Path::new(chosen));
            assert_eq!((n, e), (PathBuf::from("out.nodes.csv"), PathBuf::from("out.edges.csv")), "{chosen}");
        }
    }

    #[test]
    fn cells_render_entities_compactly() {
        let mut n = Node::new("h1");
        n.labels.insert("Host".into());
        assert_eq!(cell_text(&ResultValue::Node(n)), "(h1:Host)");
        let e = Edge::new("e1", "a", "b", "KNOWS");
        assert_eq!(cell_text(&ResultValue::Edge(e)), "[e1:KNOWS a→b]");
        assert_eq!(cell_text(&ResultValue::List(vec![ResultValue::Int(1), ResultValue::Null])), "[1, null]");
    }
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod ui_tests;
