//! Small persisted preferences (`%APPDATA%\GrapHydra\studio.json`).

use std::path::PathBuf;

use serde_json::json;

const MAX_RECENT: usize = 10;

#[derive(Default)]
pub struct Settings {
    pub dark: bool,
    pub recent: Vec<PathBuf>,
    pub history: Vec<String>,
}

fn file() -> Option<PathBuf> {
    // Tests never read or write the user's real preferences.
    if cfg!(test) {
        return None;
    }
    // Portable installs can keep preferences next to the executable.
    if let Some(p) = std::env::var_os("GRAPHYDRA_STUDIO_SETTINGS") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("APPDATA").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(base).join("GrapHydra").join("studio.json"))
}

impl Settings {
    pub fn load() -> Settings {
        let mut s = Settings { dark: true, ..Settings::default() };
        let Some(text) = file().and_then(|f| std::fs::read_to_string(f).ok()) else { return s };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return s };
        s.dark = v["dark"].as_bool().unwrap_or(true);
        let strings = |key: &str| -> Vec<String> {
            v[key]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default()
        };
        s.recent = strings("recent").into_iter().map(PathBuf::from).collect();
        s.history = strings("history");
        s
    }

    /// Best effort: preferences are a convenience, never worth an error dialog.
    pub fn save(&self) {
        let Some(f) = file() else { return };
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let v = json!({
            "dark": self.dark,
            "recent": self.recent.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "history": self.history,
        });
        let _ = std::fs::write(f, v.to_string());
    }

    pub fn remember_file(&mut self, path: &std::path::Path) {
        let path = path.to_path_buf();
        self.recent.retain(|p| p != &path);
        self.recent.insert(0, path);
        self.recent.truncate(MAX_RECENT);
        self.save();
    }

    pub fn remember_query(&mut self, q: &str) {
        let q = q.trim().to_string();
        if q.is_empty() {
            return;
        }
        self.history.retain(|h| h != &q);
        self.history.insert(0, q);
        self.history.truncate(50);
        self.save();
    }
}
