//! User-facing texts, kept separate from the UI code so a future
//! translation only adds another [`Strings`] table.
//!
//! The application holds a `&'static Strings`; every label, button,
//! hint and message is read through it. English ([`EN`]) is the
//! default and reference language. Texts that embed values use a
//! `{placeholder}` template field plus a small interpolation method —
//! translators override the template, never the code.

/// All user-visible text of the GUI.
#[derive(Debug)]
pub struct Strings {
    /// Application and window title.
    pub app_title: &'static str,

    // -- Project list ---------------------------------------------------
    pub projects: &'static str,
    pub new_project: &'static str,
    pub no_projects_hint: &'static str,
    pub select_project_hint: &'static str,

    // -- Project status ---------------------------------------------------
    pub status_never_built: &'static str,
    pub status_rebuild_needed: &'static str,
    pub status_up_to_date: &'static str,

    // -- Project details ----------------------------------------------------
    pub build_index: &'static str,
    pub update_index: &'static str,
    pub edit: &'static str,
    pub delete: &'static str,
    pub settings_section: &'static str,
    pub source_roots: &'static str,
    pub root_recursive: &'static str,
    pub root_top_level_only: &'static str,
    pub excluded_dirs: &'static str,
    pub excluded_extensions: &'static str,
    pub respect_gitignore: &'static str,
    pub max_indexed_file_size: &'static str,
    pub index_archives: &'static str,
    pub archive_max_depth: &'static str,

    // -- Build progress -------------------------------------------------------
    pub cancel_build: &'static str,
    pub starting: &'static str,
    pub files_seen: &'static str,
    pub files_indexed: &'static str,
    pub files_ignored: &'static str,
    pub errors: &'static str,
    pub archives: &'static str,
    pub archive_entries: &'static str,
    pub bytes_read: &'static str,

    // -- Build summary ----------------------------------------------------------
    pub last_build: &'static str,
    pub kind: &'static str,
    pub kind_full: &'static str,
    pub kind_update: &'static str,
    pub duration: &'static str,
    pub top_extensions: &'static str,
    pub ignored_by_extension: &'static str,
    pub ignored_by_sniff: &'static str,
    pub too_large: &'static str,
    pub security_limits: &'static str,
    pub archives_processed: &'static str,
    pub archive_entries_indexed: &'static str,
    pub delta_added: &'static str,
    pub delta_removed: &'static str,
    pub delta_updated: &'static str,
    pub yes: &'static str,
    pub no: &'static str,

    // -- Project editor ------------------------------------------------------------
    pub new_project_title: &'static str,
    pub edit_project_title: &'static str,
    pub name: &'static str,
    pub project_name_hint: &'static str,
    pub browse: &'static str,
    pub add_root: &'static str,
    pub remove_root: &'static str,
    pub root_path_hint: &'static str,
    pub excluded_dirs_hint: &'static str,
    pub excluded_extensions_hint: &'static str,
    pub save: &'static str,
    pub cancel: &'static str,
    pub err_name_required: &'static str,
    pub err_non_unicode_path: &'static str,

    // -- Delete confirmation ---------------------------------------------------------
    pub delete_project_title: &'static str,
    pub delete_warning: &'static str,

    // -- Catalog ------------------------------------------------------------------------
    pub catalog_unavailable: &'static str,
    pub retry: &'static str,

    // -- Messages (templates; use the interpolation methods) ------------------------------
    pub delete_confirm_template: &'static str,
    pub project_created_template: &'static str,
    pub project_updated: &'static str,
    pub project_deleted_template: &'static str,
    pub build_completed_template: &'static str,
    pub build_cancelled: &'static str,
    pub build_failed_template: &'static str,
}

impl Strings {
    /// "Delete project "{name}"?"
    pub fn delete_confirm(&self, name: &str) -> String {
        self.delete_confirm_template.replace("{name}", name)
    }

    /// "Project "{name}" created."
    pub fn project_created(&self, name: &str) -> String {
        self.project_created_template.replace("{name}", name)
    }

    /// "Project "{name}" deleted."
    pub fn project_deleted(&self, name: &str) -> String {
        self.project_deleted_template.replace("{name}", name)
    }

    /// "Build completed in {secs} s — {files} files indexed."
    pub fn build_completed(&self, files: usize, duration: std::time::Duration) -> String {
        self.build_completed_template
            .replace("{secs}", &format!("{:.1}", duration.as_secs_f64()))
            .replace("{files}", &files.to_string())
    }

    /// "Build failed: {message}"
    pub fn build_failed(&self, message: &str) -> String {
        self.build_failed_template.replace("{message}", message)
    }
}

/// English text table — the default and reference language.
pub static EN: Strings = Strings {
    app_title: "rsearch",

    projects: "Projects",
    new_project: "+ New project",
    no_projects_hint: "No projects yet.\nCreate one to start indexing.",
    select_project_hint: "Select a project, or create a new one.",

    status_never_built: "Never built",
    status_rebuild_needed: "Rebuild needed",
    status_up_to_date: "Up to date",

    build_index: "Build index",
    update_index: "Update index",
    edit: "Edit…",
    delete: "Delete…",
    settings_section: "Settings",
    source_roots: "Source roots",
    root_recursive: "recursive",
    root_top_level_only: "top level only",
    excluded_dirs: "Excluded directories",
    excluded_extensions: "Excluded extensions",
    respect_gitignore: "Respect .gitignore files",
    max_indexed_file_size: "Max indexed file size",
    index_archives: "Index archive contents (.zip, .jar, …)",
    archive_max_depth: "Archive nesting depth",

    cancel_build: "Cancel build",
    starting: "Starting…",
    files_seen: "Files seen",
    files_indexed: "Files indexed",
    files_ignored: "Files ignored",
    errors: "Errors",
    archives: "Archives",
    archive_entries: "Archive entries",
    bytes_read: "Bytes read",

    last_build: "Last build",
    kind: "Kind",
    kind_full: "Full rebuild",
    kind_update: "Incremental update",
    duration: "Duration",
    top_extensions: "Top extensions",
    ignored_by_extension: "Ignored by extension",
    ignored_by_sniff: "Ignored by sniffing",
    too_large: "Too large",
    security_limits: "Security limits",
    archives_processed: "Archives processed",
    archive_entries_indexed: "Archive entries indexed",
    delta_added: "added",
    delta_removed: "removed",
    delta_updated: "updated",
    yes: "yes",
    no: "no",

    new_project_title: "New project",
    edit_project_title: "Edit project",
    name: "Name",
    project_name_hint: "Project name",
    browse: "Browse…",
    add_root: "+ Add root",
    remove_root: "Remove this root",
    root_path_hint: "Directory path, e.g. C:\\src\\my-project",
    excluded_dirs_hint: "One per line, or separated by commas",
    excluded_extensions_hint: "e.g. log, tmp, bak",
    save: "Save",
    cancel: "Cancel",
    err_name_required: "A project name is required.",
    err_non_unicode_path: "The selected path is not valid Unicode and cannot be used.",

    delete_project_title: "Delete project",
    delete_warning: "Its index files will be removed from disk. This cannot be undone.",

    catalog_unavailable: "The project catalog could not be opened",
    retry: "Retry",

    delete_confirm_template: "Delete project \"{name}\"?",
    project_created_template: "Project \"{name}\" created.",
    project_updated: "Project updated.",
    project_deleted_template: "Project \"{name}\" deleted.",
    build_completed_template: "Build completed in {secs} s — {files} files indexed.",
    build_cancelled: "Build cancelled. The previous index is unchanged.",
    build_failed_template: "Build failed: {message}",
};
