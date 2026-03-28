use std::borrow::Cow;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const GLOBAL_SETTINGS_FILE: &str = "tui-settings.json";
const WORKSPACE_REGISTRY_FILE: &str = "workspaces.json";
const CONVERSATIONS_DIR: &str = "conversations";
const CODEX_API_HOME_DIR: &str = "codex-api-home";
const REPO_ANTIPHON_DIR: &str = ".antiphon";
const LEGACY_REPO_SETTINGS_FILE: &str = ".antiphon.tui-settings.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsScope {
    Global,
    RepoLocal,
}

impl SettingsScope {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::RepoLocal => "repo",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    pub runtime_home: PathBuf,
    pub global_settings_path: PathBuf,
    pub global_conversations_dir: PathBuf,
    pub workspace_registry_path: PathBuf,
    pub codex_api_home: PathBuf,
}

impl RuntimePaths {
    pub fn new(runtime_home: PathBuf) -> Self {
        Self {
            global_settings_path: runtime_home.join(GLOBAL_SETTINGS_FILE),
            global_conversations_dir: runtime_home.join(CONVERSATIONS_DIR),
            workspace_registry_path: runtime_home.join(WORKSPACE_REGISTRY_FILE),
            codex_api_home: runtime_home.join(CODEX_API_HOME_DIR),
            runtime_home,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePaths {
    pub workspace_root: PathBuf,
    pub repo_antiphon_dir: PathBuf,
    pub settings_scope: SettingsScope,
    pub settings_path: PathBuf,
    pub conversations_dir: PathBuf,
}

impl WorkspacePaths {
    pub fn for_workspace(
        runtime: &RuntimePaths,
        workspace_root: PathBuf,
        settings_scope: SettingsScope,
    ) -> Self {
        let repo_antiphon_dir = workspace_root.join(REPO_ANTIPHON_DIR);
        match settings_scope {
            SettingsScope::Global => Self {
                workspace_root,
                repo_antiphon_dir,
                settings_scope,
                settings_path: runtime.global_settings_path.clone(),
                conversations_dir: runtime.global_conversations_dir.clone(),
            },
            SettingsScope::RepoLocal => Self {
                workspace_root,
                settings_scope,
                settings_path: repo_antiphon_dir.join(GLOBAL_SETTINGS_FILE),
                conversations_dir: repo_antiphon_dir.join(CONVERSATIONS_DIR),
                repo_antiphon_dir,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePreference {
    pub workspace_root: PathBuf,
    pub preferred_scope: SettingsScope,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRegistry {
    pub last_workspace: Option<PathBuf>,
    #[serde(default)]
    pub recent_workspaces: Vec<PathBuf>,
    #[serde(default)]
    pub workspace_preferences: Vec<WorkspacePreference>,
}

impl WorkspaceRegistry {
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = serde_json::to_string_pretty(self)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        fs::write(path, payload)
    }

    pub fn preferred_scope(&self, workspace_root: &Path) -> Option<SettingsScope> {
        self.workspace_preferences
            .iter()
            .find(|preference| preference.workspace_root == workspace_root)
            .map(|preference| preference.preferred_scope)
    }

    pub fn remember_workspace(&mut self, workspace_root: &Path) {
        self.last_workspace = Some(workspace_root.to_path_buf());
        self.recent_workspaces.retain(|path| path != workspace_root);
        self.recent_workspaces
            .insert(0, workspace_root.to_path_buf());
        self.recent_workspaces.truncate(10);
    }

    pub fn set_preference(&mut self, workspace_root: &Path, preferred_scope: SettingsScope) {
        if let Some(preference) = self
            .workspace_preferences
            .iter_mut()
            .find(|preference| preference.workspace_root == workspace_root)
        {
            preference.preferred_scope = preferred_scope;
            return;
        }

        self.workspace_preferences.push(WorkspacePreference {
            workspace_root: workspace_root.to_path_buf(),
            preferred_scope,
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSuggestion {
    pub path: PathBuf,
    pub display: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeResolution {
    Resolved(SettingsScope),
    NeedsChoice { has_legacy_repo_settings: bool },
}

pub fn initial_workspace_root(
    cli_workspace: Option<&Path>,
    registry: &WorkspaceRegistry,
) -> PathBuf {
    if let Some(path) = cli_workspace {
        return canonicalize_or_absolute(path);
    }
    if let Some(path) = registry.last_workspace.as_deref() {
        return canonicalize_or_absolute(path);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn resolve_scope(registry: &WorkspaceRegistry, workspace_root: &Path) -> ScopeResolution {
    if repo_local_settings_path(workspace_root).exists() {
        return ScopeResolution::Resolved(SettingsScope::RepoLocal);
    }
    if let Some(scope) = registry.preferred_scope(workspace_root) {
        return ScopeResolution::Resolved(scope);
    }
    ScopeResolution::NeedsChoice {
        has_legacy_repo_settings: legacy_repo_settings_path(workspace_root).exists(),
    }
}

pub fn normalize_workspace_path(raw: &str) -> io::Result<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace path cannot be empty",
        ));
    }

    let expanded = expand_tilde(trimmed);
    let normalized = canonicalize_or_absolute(Path::new(expanded.as_ref()));
    let meta = fs::metadata(&normalized).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("workspace path does not exist: {}", normalized.display()),
        )
    })?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "workspace path is not a directory: {}",
                normalized.display()
            ),
        ));
    }
    Ok(normalized)
}

pub fn bootstrap_settings_file(
    workspace_paths: &WorkspacePaths,
    current_effective_settings: Option<&str>,
    default_settings: &str,
) -> io::Result<()> {
    if workspace_paths.settings_path.exists() {
        return Ok(());
    }

    if workspace_paths.settings_scope == SettingsScope::RepoLocal {
        fs::create_dir_all(&workspace_paths.repo_antiphon_dir)?;
        fs::create_dir_all(&workspace_paths.conversations_dir)?;
    } else if let Some(parent) = workspace_paths.settings_path.parent() {
        fs::create_dir_all(parent)?;
        fs::create_dir_all(&workspace_paths.conversations_dir)?;
    }

    let payload = current_effective_settings.unwrap_or(default_settings);
    fs::write(&workspace_paths.settings_path, payload)
}

pub fn closest_workspace_suggestions(
    typed_input: &str,
    registry: &WorkspaceRegistry,
) -> Vec<WorkspaceSuggestion> {
    let expanded = expand_tilde(typed_input.trim());
    let input_path = PathBuf::from(expanded.as_ref());
    let nearest_existing_parent = find_nearest_existing_parent(&input_path);
    let query = input_path
        .file_name()
        .and_then(|segment| segment.to_str())
        .unwrap_or(expanded.as_ref());
    let query_lower = query.to_ascii_lowercase();

    let mut candidates: Vec<PathBuf> = registry.recent_workspaces.clone();
    if let Some(parent) = nearest_existing_parent {
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    candidates.push(path);
                }
            }
        }
    }

    candidates.sort();
    candidates.dedup();

    let mut scored: Vec<(usize, PathBuf)> = candidates
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|segment| segment.to_str())
                .unwrap_or("");
            let name_lower = name.to_ascii_lowercase();
            let path_display = path.display().to_string().to_ascii_lowercase();
            let score = fuzzy_score(&query_lower, &name_lower, &path_display);
            (score, canonicalize_or_absolute(&path))
        })
        .filter(|(score, _)| *score < usize::MAX)
        .collect();

    scored.sort_by(|(score_a, path_a), (score_b, path_b)| {
        score_a
            .cmp(score_b)
            .then_with(|| path_a.as_os_str().cmp(path_b.as_os_str()))
    });
    scored.truncate(5);

    scored
        .into_iter()
        .map(|(_, path)| WorkspaceSuggestion {
            display: path.display().to_string(),
            path,
        })
        .collect()
}

pub fn repo_local_settings_path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(REPO_ANTIPHON_DIR)
        .join(GLOBAL_SETTINGS_FILE)
}

pub fn legacy_repo_settings_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(LEGACY_REPO_SETTINGS_FILE)
}

pub fn import_legacy_repo_settings(workspace_root: &Path) -> io::Result<PathBuf> {
    let source = legacy_repo_settings_path(workspace_root);
    let dest = repo_local_settings_path(workspace_root);
    if !source.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("legacy settings file not found: {}", source.display()),
        ));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::copy(&source, &dest)?;
    Ok(dest)
}

fn canonicalize_or_absolute(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

fn expand_tilde(input: &str) -> Cow<'_, str> {
    if input == "~" || input.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            let suffix = input.strip_prefix('~').unwrap_or_default();
            return Cow::Owned(format!("{}{}", home.display(), suffix));
        }
    }
    Cow::Borrowed(input)
}

fn find_nearest_existing_parent(path: &Path) -> Option<&Path> {
    let mut cursor = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };
    while let Some(candidate) = cursor {
        if candidate.exists() {
            return Some(candidate);
        }
        cursor = candidate.parent();
    }
    None
}

fn fuzzy_score(query: &str, name: &str, path_display: &str) -> usize {
    if query.is_empty() {
        return usize::MAX;
    }
    if name == query {
        return 0;
    }
    if name.starts_with(query) {
        return 1;
    }
    if name.contains(query) {
        return 2;
    }
    if path_display.contains(query) {
        return 3;
    }
    let edit = levenshtein_distance(query, name);
    if edit <= 3 {
        return 10 + edit;
    }
    usize::MAX
}

fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr = vec![0; b_chars.len() + 1];

    for (i, a_ch) in a_chars.iter().enumerate() {
        curr[0] = i + 1;
        for (j, b_ch) in b_chars.iter().enumerate() {
            let cost = usize::from(a_ch != b_ch);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "antiphon-workspace-test-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&base).expect("temp dir");
        base
    }

    #[test]
    fn runtime_paths_anchor_global_files_under_runtime_home() {
        let root = temp_dir("runtime");
        let runtime = RuntimePaths::new(root.clone());
        assert_eq!(runtime.global_settings_path, root.join("tui-settings.json"));
        assert_eq!(runtime.global_conversations_dir, root.join("conversations"));
        assert_eq!(
            runtime.workspace_registry_path,
            root.join("workspaces.json")
        );
        assert_eq!(runtime.codex_api_home, root.join("codex-api-home"));
    }

    #[test]
    fn workspace_paths_resolve_global_scope_to_runtime_home() {
        let runtime = RuntimePaths::new(temp_dir("global"));
        let repo = temp_dir("repo");
        let paths = WorkspacePaths::for_workspace(&runtime, repo.clone(), SettingsScope::Global);
        assert_eq!(paths.workspace_root, repo);
        assert_eq!(paths.settings_path, runtime.global_settings_path);
        assert_eq!(paths.conversations_dir, runtime.global_conversations_dir);
    }

    #[test]
    fn workspace_paths_resolve_repo_scope_to_dot_antiphon() {
        let runtime = RuntimePaths::new(temp_dir("repo-local-runtime"));
        let repo = temp_dir("repo-local");
        let paths = WorkspacePaths::for_workspace(&runtime, repo.clone(), SettingsScope::RepoLocal);
        assert_eq!(paths.workspace_root, repo.clone());
        assert_eq!(
            paths.settings_path,
            repo.join(".antiphon/tui-settings.json")
        );
        assert_eq!(
            paths.conversations_dir,
            repo.join(".antiphon/conversations")
        );
    }

    #[test]
    fn registry_round_trip_preserves_preferences() {
        let dir = temp_dir("registry");
        let path = dir.join("workspaces.json");
        let repo = dir.join("repo");
        let mut registry = WorkspaceRegistry::default();
        registry.remember_workspace(&repo);
        registry.set_preference(&repo, SettingsScope::RepoLocal);
        registry.save(&path).expect("save");

        let loaded = WorkspaceRegistry::load(&path).expect("load");
        assert_eq!(loaded.last_workspace, Some(repo.clone()));
        assert_eq!(loaded.recent_workspaces, vec![repo.clone()]);
        assert_eq!(
            loaded.preferred_scope(&repo),
            Some(SettingsScope::RepoLocal)
        );
    }

    #[test]
    fn scope_resolution_prefers_repo_local_settings_file() {
        let repo = temp_dir("scope-priority");
        fs::create_dir_all(repo.join(".antiphon")).expect("antiphon dir");
        fs::write(repo.join(".antiphon/tui-settings.json"), "{}").expect("settings");

        let mut registry = WorkspaceRegistry::default();
        registry.set_preference(&repo, SettingsScope::Global);

        assert_eq!(
            resolve_scope(&registry, &repo),
            ScopeResolution::Resolved(SettingsScope::RepoLocal)
        );
    }

    #[test]
    fn scope_resolution_uses_registry_when_repo_local_missing() {
        let repo = temp_dir("scope-pref");
        let mut registry = WorkspaceRegistry::default();
        registry.set_preference(&repo, SettingsScope::Global);
        assert_eq!(
            resolve_scope(&registry, &repo),
            ScopeResolution::Resolved(SettingsScope::Global)
        );
    }

    #[test]
    fn bootstrap_repo_local_settings_copies_current_effective_settings() {
        let runtime = RuntimePaths::new(temp_dir("bootstrap-runtime"));
        let repo = temp_dir("bootstrap-repo");
        let paths = WorkspacePaths::for_workspace(&runtime, repo.clone(), SettingsScope::RepoLocal);
        bootstrap_settings_file(&paths, Some("{\"prompt\":\"copied\"}"), "{}").expect("bootstrap");

        assert_eq!(
            fs::read_to_string(paths.settings_path).expect("read"),
            "{\"prompt\":\"copied\"}"
        );
        assert!(repo.join(".antiphon/conversations").exists());
    }

    #[test]
    fn normalize_workspace_path_expands_tilde_and_canonicalizes() {
        let repo = temp_dir("normalize");
        let input = repo.display().to_string();
        let normalized = normalize_workspace_path(&input).expect("normalize");
        assert!(normalized.is_absolute());
        assert_eq!(normalized, fs::canonicalize(repo).expect("canonical"));
    }

    #[test]
    fn closest_matches_rank_recent_and_sibling_dirs() {
        let root = temp_dir("suggestions");
        let alpha = root.join("alpha-repo");
        let alpine = root.join("alpine");
        let beta = root.join("beta");
        fs::create_dir_all(&alpha).expect("alpha");
        fs::create_dir_all(&alpine).expect("alpine");
        fs::create_dir_all(&beta).expect("beta");

        let mut registry = WorkspaceRegistry::default();
        registry.recent_workspaces = vec![beta.clone(), alpha.clone()];

        let typed = root.join("alp").display().to_string();
        let suggestions = closest_workspace_suggestions(&typed, &registry);

        assert!(!suggestions.is_empty());
        assert_eq!(suggestions[0].path, alpha);
        assert!(suggestions.iter().any(|entry| entry.path == alpine));
        assert!(suggestions.len() <= 5);
    }
}
