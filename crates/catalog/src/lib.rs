//! # rsearch-catalog
//!
//! Persistent catalog of named scan configurations ("projects") for
//! rsearch. The catalog is a SQLite database of its own — never the
//! same file as a project index — stored next to the application
//! executable:
//!
//! ```text
//! <exe dir>/projects.db            <- this catalog
//! <exe dir>/projects/<id>/index.db <- the project's rsearch index
//! ```
//!
//! The project `id` (a random UUID v4) owns the index path; the `name`
//! is pure display data — renaming a project touches nothing else.
//! Each row also keeps the [`BuildSummary`] of the last successful
//! build so a UI can show it without reopening the index.
//!
//! This is an application layer: `rsearch-engine` knows nothing about
//! the catalog and never persists index paths itself.

mod id;
mod settings;

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

pub use rsearch_engine::{BuildSummary, RootSpec};
pub use settings::ProjectSettings;

/// Schema of the catalog database (idempotent).
const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS projects(
    id                       TEXT PRIMARY KEY,
    name                     TEXT NOT NULL,
    created_at               INTEGER NOT NULL,
    settings_json            TEXT NOT NULL,
    last_build_settings_json TEXT,
    last_build_summary_json  TEXT,
    last_build_at            INTEGER,
    index_db_path            TEXT NOT NULL
);
";

/// Failure of a catalog operation.
#[derive(Debug)]
pub enum CatalogError {
    /// Filesystem-level failure (catalog file, project index dir).
    Io(std::io::Error),
    /// SQLite-level failure on the catalog database.
    Sqlite(rusqlite::Error),
    /// Settings or summary could not be (de)serialized.
    Serialize(serde_json::Error),
    /// The settings are invalid for a build (for example two roots
    /// naming the same directory with different `recursive` flags).
    InvalidSettings(String),
    /// No project exists with this id.
    NotFound(String),
    /// A stored row contains JSON that does not decode — the catalog
    /// was written by an incompatible version or is damaged.
    CorruptRow(String),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogError::Io(e) => write!(f, "i/o error: {e}"),
            CatalogError::Sqlite(e) => write!(f, "catalog sqlite error: {e}"),
            CatalogError::Serialize(e) => write!(f, "serialization error: {e}"),
            CatalogError::InvalidSettings(m) => write!(f, "invalid project settings: {m}"),
            CatalogError::NotFound(id) => write!(f, "project not found: {id}"),
            CatalogError::CorruptRow(m) => write!(f, "corrupt catalog row: {m}"),
        }
    }
}

impl std::error::Error for CatalogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CatalogError::Io(e) => Some(e),
            CatalogError::Sqlite(e) => Some(e),
            CatalogError::Serialize(e) => Some(e),
            _ => None,
        }
    }
}

/// One catalog row, fully decoded.
#[derive(Debug, Clone)]
pub struct Project {
    /// Stable identifier (UUID v4), also the root of `index_db_path`.
    pub id: String,
    /// Display name — free text, unrelated to the index location.
    pub name: String,
    /// Unix timestamp (seconds) of project creation.
    pub created_at: i64,
    /// Current desired settings.
    pub settings: ProjectSettings,
    /// Settings used by the last successful build; `None` if never built.
    pub last_build_settings: Option<ProjectSettings>,
    /// Summary of the last successful build; `None` if never built.
    pub last_build_summary: Option<BuildSummary>,
    /// Unix timestamp (seconds) of the last successful build.
    pub last_build_at: Option<i64>,
    /// Index file for this project — derived from `id` at creation.
    pub index_db_path: PathBuf,
}

/// The project catalog: one SQLite connection plus the base directory
/// holding `projects.db` and the `projects/<id>/` index directories.
pub struct Catalog {
    conn: Connection,
    /// Directory containing `projects.db`; index directories live in
    /// `<base_dir>/projects/<id>/`.
    base_dir: PathBuf,
}

impl Catalog {
    /// Opens (creating if needed) the catalog database at `path`. The
    /// directory containing `path` becomes the base directory under
    /// which `projects/<id>/index.db` files live.
    pub fn open(path: impl AsRef<Path>) -> Result<Catalog, CatalogError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(CatalogError::Io)?;
            }
        }
        let conn = Connection::open(path).map_err(CatalogError::Sqlite)?;
        conn.execute_batch(SCHEMA_SQL)
            .map_err(CatalogError::Sqlite)?;
        Ok(Catalog {
            conn,
            base_dir: path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from(".")),
        })
    }

    /// Opens the catalog at the default location: `projects.db` in the
    /// directory containing the running executable.
    pub fn open_default() -> Result<Catalog, CatalogError> {
        let exe = std::env::current_exe().map_err(CatalogError::Io)?;
        let dir = exe.parent().ok_or_else(|| {
            CatalogError::Io(std::io::Error::other("executable has no parent directory"))
        })?;
        Catalog::open(dir.join("projects.db"))
    }

    /// Directory under which per-project index directories live.
    pub fn projects_dir(&self) -> PathBuf {
        self.base_dir.join("projects")
    }

    /// Creates a project: validates the settings, generates a random
    /// id, derives `index_db_path` from it and inserts the row. The
    /// project's index directory is created so a first build can write
    /// into it directly.
    pub fn create_project(
        &self,
        name: impl Into<String>,
        settings: ProjectSettings,
    ) -> Result<Project, CatalogError> {
        settings.validate().map_err(CatalogError::InvalidSettings)?;

        let id = id::new_project_id(&self.conn)?;
        let index_db_path = self.projects_dir().join(&id).join("index.db");
        if let Some(dir) = index_db_path.parent() {
            std::fs::create_dir_all(dir).map_err(CatalogError::Io)?;
        }
        // The stored path is reopened later (needs_rebuild, delete):
        // never a lossy conversion — a non-Unicode path is refused.
        let index_db_str = index_db_path
            .to_str()
            .ok_or_else(|| {
                CatalogError::Io(std::io::Error::other("index path is not valid Unicode"))
            })?
            .to_owned();
        let settings_json = serde_json::to_string(&settings).map_err(CatalogError::Serialize)?;
        let name = name.into();
        let created_at = unix_now();
        self.conn
            .execute(
                "INSERT INTO projects(
                    id, name, created_at, settings_json,
                    last_build_settings_json, last_build_summary_json,
                    last_build_at, index_db_path
                 ) VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, ?5)",
                params![id, name, created_at, settings_json, index_db_str],
            )
            .map_err(CatalogError::Sqlite)?;

        Ok(Project {
            id,
            name,
            created_at,
            settings,
            last_build_settings: None,
            last_build_summary: None,
            last_build_at: None,
            index_db_path,
        })
    }

    /// Replaces the desired settings of a project. Only `settings_json`
    /// is touched; a settings change invalidates nothing by itself —
    /// [`Catalog::needs_rebuild`] reports the drift.
    pub fn update_project_settings(
        &self,
        project_id: &str,
        settings: ProjectSettings,
    ) -> Result<(), CatalogError> {
        settings.validate().map_err(CatalogError::InvalidSettings)?;
        let settings_json = serde_json::to_string(&settings).map_err(CatalogError::Serialize)?;
        let n = self
            .conn
            .execute(
                "UPDATE projects SET settings_json = ?2 WHERE id = ?1",
                params![project_id, settings_json],
            )
            .map_err(CatalogError::Sqlite)?;
        if n == 0 {
            return Err(CatalogError::NotFound(project_id.to_string()));
        }
        Ok(())
    }

    /// Renames a project. Only `name` changes — the index path derives
    /// from `id` and never moves.
    pub fn rename_project(&self, project_id: &str, name: &str) -> Result<(), CatalogError> {
        let n = self
            .conn
            .execute(
                "UPDATE projects SET name = ?2 WHERE id = ?1",
                params![project_id, name],
            )
            .map_err(CatalogError::Sqlite)?;
        if n == 0 {
            return Err(CatalogError::NotFound(project_id.to_string()));
        }
        Ok(())
    }

    /// Deletes a project: the index directory first, the catalog row
    /// second — so an interruption never leaves a row pointing at a
    /// deleted index.
    pub fn delete_project(&self, project_id: &str) -> Result<(), CatalogError> {
        let project = self.get_project(project_id)?;
        // Files first: index.db plus any sidecars (.building, -wal…).
        let index_dir = self.projects_dir().join(&project.id);
        match std::fs::remove_dir_all(&index_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(CatalogError::Io(e)),
        }
        self.conn
            .execute("DELETE FROM projects WHERE id = ?1", params![project_id])
            .map_err(CatalogError::Sqlite)?;
        Ok(())
    }

    /// Lists every project in creation order.
    pub fn list_projects(&self) -> Result<Vec<Project>, CatalogError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, name, created_at, settings_json,
                        last_build_settings_json, last_build_summary_json,
                        last_build_at, index_db_path
                 FROM projects ORDER BY created_at, rowid",
            )
            .map_err(CatalogError::Sqlite)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, String>(7)?,
                ))
            })
            .map_err(CatalogError::Sqlite)?;
        let mut projects = Vec::new();
        for row in rows {
            let (id, name, created_at, settings_json, lbs, lbsum, lba, idx) =
                row.map_err(CatalogError::Sqlite)?;
            projects.push(Project {
                id: id.clone(),
                name,
                created_at,
                settings: decode_settings(&settings_json)?,
                last_build_settings: lbs.as_deref().map(decode_settings).transpose()?,
                last_build_summary: lbsum.as_deref().map(decode_summary).transpose()?,
                last_build_at: lba,
                index_db_path: PathBuf::from(idx),
            });
        }
        Ok(projects)
    }

    /// Loads one project by id.
    pub fn get_project(&self, project_id: &str) -> Result<Project, CatalogError> {
        self.list_projects()?
            .into_iter()
            .find(|p| p.id == project_id)
            .ok_or_else(|| CatalogError::NotFound(project_id.to_string()))
    }

    /// Whether a project's index needs (re)building: never built,
    /// settings drifted from the last build's snapshot (structural
    /// comparison — JSON formatting is irrelevant), or the index file
    /// is absent from disk. A rename alone never triggers a rebuild.
    pub fn needs_rebuild(&self, project: &Project) -> bool {
        let Some(last) = &project.last_build_settings else {
            return true;
        };
        if *last != project.settings {
            return true;
        }
        !project.index_db_path.exists()
    }

    /// Records the result of a successful `rebuild_index` /
    /// `update_index`: snapshots the settings actually used, stores the
    /// build summary for instant display, and stamps the build time.
    pub fn record_build_result(
        &self,
        project_id: &str,
        settings_used: &ProjectSettings,
        summary: &BuildSummary,
    ) -> Result<(), CatalogError> {
        let settings_json =
            serde_json::to_string(settings_used).map_err(CatalogError::Serialize)?;
        let summary_json = serde_json::to_string(summary).map_err(CatalogError::Serialize)?;
        let n = self
            .conn
            .execute(
                "UPDATE projects SET
                    last_build_settings_json = ?2,
                    last_build_summary_json = ?3,
                    last_build_at = ?4
                 WHERE id = ?1",
                params![project_id, settings_json, summary_json, unix_now()],
            )
            .map_err(CatalogError::Sqlite)?;
        if n == 0 {
            return Err(CatalogError::NotFound(project_id.to_string()));
        }
        Ok(())
    }
}

fn decode_settings(json: &str) -> Result<ProjectSettings, CatalogError> {
    serde_json::from_str(json).map_err(|e| CatalogError::CorruptRow(format!("settings_json: {e}")))
}

fn decode_summary(json: &str) -> Result<BuildSummary, CatalogError> {
    serde_json::from_str(json)
        .map_err(|e| CatalogError::CorruptRow(format!("last_build_summary_json: {e}")))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
