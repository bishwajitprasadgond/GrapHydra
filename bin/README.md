# Ready-to-run Windows executables

| File | What it is |
|---|---|
| [`graphydra-studio.exe`](graphydra-studio.exe) | **GrapHydra Studio**, the desktop app: create databases, see and explore the graph, edit it, and run queries |
| [`graphydra.exe`](graphydra.exe) | The **`graphydra` command-line tool**: `init`, `query`, `import`, `export`, `shell`, and more |

Both are for 64-bit Windows 10 and 11. They are self-contained: nothing to
install, no Python or Rust needed. Version: **0.1.0**.

## Run GrapHydra Studio

1. Download [`graphydra-studio.exe`](graphydra-studio.exe) (on GitHub, open
   the file and click the download button).
2. Double-click it.
3. Choose **File → New sample database…** to explore example data, or
   **File → Open…** to open a `.graph` file.

You can also open a file directly: drag a `.graph` file onto the `.exe`, or
run:

```powershell
.\graphydra-studio.exe C:\path\to\library.graph
```

The full guide is in [docs/studio.md](../docs/studio.md).

### "Windows protected your PC"

The executables are not code-signed, so the first time you run one, Windows
SmartScreen may show this warning. Click **More info → Run anyway**. Only
do this for files downloaded from this repository.

## Run the command-line tool

```powershell
.\graphydra.exe --help
.\graphydra.exe init library.graph
.\graphydra.exe query library.graph "MATCH (n) RETURN n LIMIT 10"
.\graphydra.exe shell library.graph
```

To use `graphydra` from any folder, copy it to a folder on your `PATH`.

## Updating these files

They are built from the source in this repository. After changing the
code, rebuild and copy them here (PowerShell, from the repository root):

```powershell
cargo build --release -p graphydra-studio -p graphydra-cli
Copy-Item target\release\graphydra-studio.exe, target\release\graphydra.exe bin\
```

Remember to also update the version in the root `Cargo.toml` when you
publish a new release.
