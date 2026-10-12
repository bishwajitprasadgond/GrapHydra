//! Headless UI tests: drive the real app through its accessibility tree.

use super::*;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};

struct Fixture {
    dir: PathBuf,
    path: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn sample(name: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("gv-studio-ui-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sample.graph");
    let mut g = Graph::create(&path).unwrap();
    crate::demo::populate(&mut g).unwrap();
    g.close().unwrap();
    Fixture { dir, path }
}

fn harness(path: &Path) -> Harness<'static, StudioApp> {
    let path = path.to_path_buf();
    let mut h = Harness::builder().with_size([1400.0, 860.0]).build_eframe(move |cc| StudioApp::new(cc, Some(path)));
    h.run_steps(3);
    h
}

/// Wait for the background query worker, stepping frames meanwhile.
fn wait_for_query(h: &mut Harness<'_, StudioApp>) {
    for _ in 0..500 {
        h.step();
        if h.state().running.is_none() {
            h.run_steps(2);
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("query did not finish");
}

/// Panels size themselves over a few frames; click only once layout is stable.
fn settle(h: &mut Harness<'_, StudioApp>) {
    h.run_steps(5);
}

fn graph_of(h: &Harness<'_, StudioApp>) -> Arc<Mutex<Graph>> {
    Arc::clone(&h.state().db.as_ref().unwrap().graph)
}

#[test]
fn opening_a_file_shows_schema_and_graph() {
    let f = sample("open");
    let h = harness(&f.path);
    h.get_by_label("Labels (9)");
    h.get_by_label("Relationship types (8)");
    h.get_by_label(":Host");
    h.get_by_label("11 nodes · 11 relationships shown");
    assert_eq!(h.state().view.node_count(), 11);
}

#[test]
fn query_tab_runs_queries_in_the_background_and_draws_results() {
    let f = sample("query");
    let mut h = harness(&f.path);
    h.get_by_label("Query").click();
    h.run_steps(2);
    h.state_mut().query = "MATCH (u:User)-[r:LOGGED_INTO]->(h:Host) RETURN u, r, h".into();
    h.get_by_label("▶ Run").click();
    h.step();
    wait_for_query(&mut h);
    let result = h.state().result.as_ref().expect("result");
    assert_eq!((result.columns.len(), result.rows.len()), (3, 2));
    h.get_by_label("2 row(s)");
    h.get_by_label("Show in graph").click();
    h.run_steps(2);
    assert_eq!((h.state().view.node_count(), h.state().view.edge_count()), (4, 2));
    assert!(h.state().tab == Tab::Graph);
}

#[test]
fn query_errors_are_shown_not_swallowed() {
    let f = sample("qerr");
    let mut h = harness(&f.path);
    h.get_by_label("Query").click();
    h.run_steps(2);
    h.state_mut().query = "MATCH (n RETURN n".into();
    h.get_by_label("▶ Run").click();
    h.step();
    wait_for_query(&mut h);
    let err = h.state().query_error.clone().expect("error");
    assert!(err.contains("syntax"), "{err}");
}

#[test]
fn read_only_checkbox_blocks_writes() {
    let f = sample("ro");
    let mut h = harness(&f.path);
    h.get_by_label("Query").click();
    h.run_steps(2);
    h.get_by_label("Read-only").click();
    h.state_mut().query = "CREATE (n:Temp) RETURN n".into();
    h.run_steps(1);
    h.get_by_label("▶ Run").click();
    h.step();
    wait_for_query(&mut h);
    assert!(h.state().query_error.is_some());
    assert_eq!(lock(&graph_of(&h)).node_count(), 11);
}

#[test]
fn create_node_form_persists_to_disk() {
    let f = sample("create");
    let mut h = harness(&f.path);
    h.get_by_label("+ Node").click();
    h.run_steps(2);
    {
        let form = h.state_mut().node_form.as_mut().expect("form open");
        form.id = "p-ada".into();
        form.labels = "Person, Engineer".into();
        form.props = PropRows(vec![("name".into(), "Ada".into()), ("born".into(), "1815".into())]);
    }
    h.run_steps(1);
    h.get_by_label("Create node").click();
    h.run_steps(2);
    assert!(h.state().node_form.is_none(), "form closes on success");
    assert_eq!(h.state().view.selected, Some(Selection::Node("p-ada".into())));
    h.get_by_label("Apply changes"); // the inspector now shows the new node
    drop(h); // closing the app closes the database
    let g = Graph::open_read_only(&f.path).unwrap();
    let n = g.get_node("p-ada").unwrap();
    assert!(n.labels.contains("Person") && n.labels.contains("Engineer"));
    assert_eq!(n.properties["born"], Value::Int(1815));
}

#[test]
fn inspector_edits_and_removes_properties() {
    let f = sample("inspect");
    let mut h = harness(&f.path);
    h.state_mut().view.selected = Some(Selection::Node("web-01".into()));
    settle(&mut h);
    {
        let rows = &mut h.state_mut().inspector.props.0;
        rows.retain(|(k, _)| k != "ip");
        rows.push(("patched".into(), "true".into()));
    }
    h.run_steps(1);
    h.get_by_label("Apply changes").click();
    h.run_steps(2);
    let n = lock(&graph_of(&h)).get_node("web-01").unwrap();
    assert_eq!(n.properties.get("patched"), Some(&Value::Bool(true)));
    assert!(!n.properties.contains_key("ip"), "removed rows delete the property");
    assert_eq!(n.properties["os"], Value::String("linux".into()), "untouched properties stay");
}

#[test]
fn delete_asks_for_confirmation_first() {
    let f = sample("delete");
    let mut h = harness(&f.path);
    h.state_mut().view.selected = Some(Selection::Node("ws-07".into()));
    settle(&mut h);
    h.get_by_label("Delete node…").click();
    h.run_steps(2);
    assert!(h.state().confirm.is_some(), "the button asks for confirmation");
    assert_eq!(lock(&graph_of(&h)).node_count(), 11, "nothing deleted before confirming");
    h.get_by_label("Cancel").click();
    settle(&mut h);
    assert!(h.state().confirm.is_none());
    assert_eq!(lock(&graph_of(&h)).node_count(), 11);
    h.get_by_label("Delete node…").click();
    h.run_steps(2);
    h.get_by_label("Delete").click();
    h.run_steps(2);
    let g = graph_of(&h);
    let g = lock(&g);
    assert_eq!((g.node_count(), g.contains_node("ws-07")), (10, false));
    assert_eq!(g.edge_count(), 8, "detach delete removed its 3 relationships");
}

#[test]
fn data_tab_pages_filters_and_inspects() {
    let f = sample("data");
    let mut h = harness(&f.path);
    h.get_by_label("Data").click();
    h.run_steps(3);
    assert_eq!(h.state().data.nodes.len(), 11);
    h.state_mut().data.search = "powershell".into();
    h.state_mut().data.loaded = false;
    h.run_steps(2);
    assert_eq!(h.state().data.nodes.len(), 1);
    h.get_by_label("Inspect").click();
    h.run_steps(2);
    assert_eq!(h.state().view.selected, Some(Selection::Node("p-1".into())));
}

#[test]
fn expanding_a_node_adds_its_neighbors() {
    let f = sample("expand");
    let mut h = harness(&f.path);
    h.get_by_label("Clear").click();
    h.run_steps(2);
    assert_eq!(h.state().view.node_count(), 0);
    let alice = lock(&graph_of(&h)).get_node("u-alice").unwrap();
    h.state_mut().show_nodes(vec![alice], None);
    h.state_mut().expand("u-alice");
    h.run_steps(2);
    // alice LOGGED_INTO ws-07 and OPENED invoice.docm.
    assert_eq!(h.state().view.node_count(), 3);
    assert_eq!(h.state().view.edge_count(), 2);
}

#[test]
fn transactions_roll_back_cleanly() {
    let f = sample("tx");
    let mut h = harness(&f.path);
    h.state_mut().transaction("begin");
    h.run_steps(2);
    h.get_by_label("TRANSACTION OPEN");
    assert!(lock(&graph_of(&h)).delete_node("u-bob", true).is_ok());
    h.state_mut().transaction("rollback");
    h.run_steps(2);
    assert!(h.query_by_label("TRANSACTION OPEN").is_none());
    assert!(lock(&graph_of(&h)).contains_node("u-bob"));
}

#[test]
fn locked_file_falls_back_to_read_only() {
    let f = sample("lock");
    let writer = Graph::open(&f.path).unwrap();
    let mut h = harness(&f.path);
    h.run_steps(2);
    assert!(h.state().db.as_ref().is_some_and(|d| d.read_only));
    h.get_by_label(" READ-ONLY ");
    assert!(h.query_by_label("+ Node").is_some_and(|n| n.accesskit_node().is_disabled()));
    drop(h);
    drop(writer);
}

/// Regenerates the README screenshots from the library example:
/// `cargo test -p graphydra-studio --release -- --ignored readme_screenshots`
#[test]
#[ignore = "renders with wgpu and rewrites docs/images; run on demand"]
fn readme_screenshots() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = root.join("docs/images");
    std::fs::create_dir_all(&out).unwrap();
    let dir = std::env::temp_dir().join(format!("gv-studio-shots-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("library.graph");
    {
        let mut g = Graph::create(&path).unwrap();
        let json = File::open(root.join("examples/library/library.json")).unwrap();
        interop::import_json(&mut g, BufReader::new(json)).unwrap();
        for (kind, prop) in [(IndexKind::Sorted, "title"), (IndexKind::Sorted, "number"), (IndexKind::Text, "text")] {
            g.create_index(IndexDef::new(kind, IndexTarget::Node, prop)).unwrap();
        }
        g.close().unwrap();
    }

    let shoot = |dark: bool, name: &str, setup: &dyn Fn(&mut Harness<'_, StudioApp>)| {
        let p = path.clone();
        // The harness applies its own theme, so pass it the one under test.
        let egui_theme = if dark { egui::Theme::Dark } else { egui::Theme::Light };
        let mut h = Harness::builder()
            .with_size([1500.0, 900.0])
            .with_pixels_per_point(1.25)
            .with_theme(egui_theme)
            .wgpu()
            .build_eframe(move |cc| {
                let mut app = StudioApp::new(cc, Some(p));
                app.settings.dark = dark;
                theme::apply(&cc.egui_ctx, dark);
                app
            });
        h.run_steps(5);
        setup(&mut h);
        let image = h.render().expect("render");
        image.save(out.join(name)).unwrap();
    };

    let graph = |h: &mut Harness<'_, StudioApp>| {
        // Let the force layout settle completely, then select a book.
        h.run_steps(600);
        h.state_mut().view.selected = Some(Selection::Node("b-archive".into()));
        h.run_steps(5);
    };
    shoot(true, "studio-graph-dark.png", &graph);
    shoot(false, "studio-graph-light.png", &graph);

    shoot(true, "studio-query-dark.png", &|h| {
        // Pointer clicks land at scaled positions under pixels_per_point != 1; set state directly.
        h.state_mut().tab = Tab::Query;
        h.run_steps(2);
        h.state_mut().query = [
            "MATCH (b:Book)-[:HAS_CHAPTER]->(c:Chapter)",
            "      -[:HAS_PAGE]->(p:Page)",
            "RETURN b.title AS book, count(DISTINCT c) AS chapters,",
            "       count(p) AS pages, sum(p.words) AS words",
            "ORDER BY words DESC",
        ]
        .join(
            "
",
        );
        let ctx = h.ctx.clone();
        h.state_mut().run_query(RunMode::Profile, &ctx);
        wait_for_query(h);
        h.run_steps(5);
    });

    shoot(true, "studio-data-dark.png", &|h| {
        h.state_mut().tab = Tab::Data;
        h.run_steps(2);
        h.state_mut().data.label = "Page".into();
        h.state_mut().data.loaded = false;
        h.run_steps(5);
    });
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Inspector width each frame (from egui's persisted panel state).
fn inspector_width(h: &Harness<'_, StudioApp>) -> f32 {
    egui::containers::panel::PanelState::load(&h.ctx, egui::Id::new("inspector")).map_or(0.0, |s| s.rect.width())
}

#[test]
fn inspector_keeps_its_width_after_double_clicking_a_node() {
    let f = sample("slide");
    let mut h = harness(&f.path);
    h.run_steps(300); // let the layout settle
    // Double-click the node "ws-07" on the canvas, as a user would.
    let pos = h.state().view.screen_pos("ws-07").expect("ws-07 is drawn");
    for _ in 0..2 {
        h.event(egui::Event::PointerMoved(pos));
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        });
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        });
        h.step();
    }
    assert_eq!(h.state().view.selected, Some(Selection::Node("ws-07".into())), "the double-click hit the node");
    let widths: Vec<f32> = (0..120)
        .map(|_| {
            h.step();
            inspector_width(&h)
        })
        .collect();
    let (min, max) = widths.iter().fold((f32::MAX, f32::MIN), |(lo, hi), w| (lo.min(*w), hi.max(*w)));
    eprintln!(
        "inspector width over 120 frames: first {:.1}, min {min:.1}, max {max:.1}, last {:.1}",
        widths[0], widths[119]
    );
    assert!(max - min < 0.5, "inspector width drifted from {min:.1} to {max:.1}");
}

#[test]
fn new_node_dialog_keeps_its_width() {
    let f = sample("dialog");
    let mut h = harness(&f.path);
    h.get_by_label("+ Node").click();
    h.run_steps(2);
    h.state_mut().node_form.as_mut().expect("form").props =
        PropRows(vec![("name".into(), "Ada".into()), ("role".into(), "engineer".into())]);
    let width = |h: &Harness<'_, StudioApp>| {
        h.ctx.memory(|m| m.area_rect(egui::Id::new("New node"))).map_or(0.0, |r| r.width())
    };
    h.run_steps(5);
    let start = width(&h);
    h.run_steps(120);
    assert!(start > 0.0, "dialog is open");
    assert!((width(&h) - start).abs() < 0.5, "dialog width drifted from {start:.1} to {:.1}", width(&h));
}
