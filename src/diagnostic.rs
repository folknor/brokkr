//! Structured compiler and cargo diagnostics shared by JSON and text readers.
//!
//! Readers keep the fields cargo emitted. Formatting, sorting and ownership
//! decisions derive from those fields at the point the report is rendered.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::rustc_text::Level;

/// A compiler or cargo diagnostic before presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    /// The lint or compiler code as emitted by cargo, without display brackets.
    pub code: Option<String>,
    pub message: String,
    pub site: Option<Site>,
    /// A compact explanatory detail such as `expected T, found U`.
    pub detail: Option<String>,
    /// Cargo's package id, when the JSON event carried one.
    pub package_id: Option<String>,
    /// Filled from workspace metadata at the reporting boundary.
    pub owner: Owner,
}

/// The primary file and span for a diagnostic. Lines and columns remain
/// optional because cargo manifest lints sometimes identify only a manifest.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Site {
    pub file: PathBuf,
    pub line: Option<u64>,
    pub column: Option<u64>,
}

/// The workspace ownership class assigned from cargo metadata.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Owner {
    /// A package declared in `workspace_members`.
    Member(String),
    /// The workspace root manifest, whose lints apply to every selection.
    WorkspaceRoot,
    /// A path outside the workspace, or a non-member cargo package id.
    NonMember(NonMemberOwner),
    /// Metadata was unavailable, or the diagnostic has no package/manifest.
    Unknown,
}

impl Owner {
    pub fn is_non_member(&self) -> bool {
        matches!(self, Self::NonMember(_))
    }
}

/// Identity retained for an outside package or manifest.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NonMemberOwner {
    Path(PathBuf),
    Package(String),
}

/// Metadata needed to classify package and manifest ownership consistently.
#[derive(Clone, Debug)]
pub struct OwnerIndex {
    workspace_root: PathBuf,
    site_root: PathBuf,
    workspace_members: HashSet<String>,
    member_manifests: HashMap<PathBuf, String>,
}

impl OwnerIndex {
    pub fn from_metadata_at(
        workspace_root: &Path,
        site_root: &Path,
        workspace_members: &HashMap<String, String>,
        member_manifests: &HashMap<PathBuf, String>,
    ) -> Self {
        Self {
            workspace_root: lexical_normalize(workspace_root),
            site_root: lexical_normalize(site_root),
            workspace_members: workspace_members.keys().cloned().collect(),
            member_manifests: member_manifests
                .iter()
                .map(|(path, id)| (lexical_normalize(path), id.clone()))
                .collect(),
        }
    }

    pub fn classify(&self, diagnostic: &Diagnostic) -> Owner {
        if let Some(path) = diagnostic.manifest_path() {
            return self.classify_path(&path, true);
        }

        if let Some(package_id) = diagnostic.package_id.as_deref() {
            if self.workspace_members.contains(package_id) {
                return Owner::Member(package_id.to_owned());
            }
            return Owner::NonMember(NonMemberOwner::Package(package_id.to_owned()));
        }
        if let Some(site) = &diagnostic.site {
            return self.classify_path(&site.file, false);
        }
        Owner::Unknown
    }

    fn classify_path(&self, path: &Path, manifest: bool) -> Owner {
        let base = if manifest {
            &self.workspace_root
        } else {
            &self.site_root
        };
        let path = if path.is_absolute() {
            lexical_normalize(path)
        } else {
            lexical_normalize(&base.join(path))
        };
        let root_manifest = self.workspace_root.join("Cargo.toml");
        if manifest && path == root_manifest {
            return Owner::WorkspaceRoot;
        }
        if manifest && let Some(id) = self.member_manifests.get(&path) {
            return Owner::Member(id.clone());
        }
        if !manifest {
            let member = self
                .member_manifests
                .iter()
                .filter_map(|(manifest, id)| Some((manifest.parent()?, id)))
                .filter(|(directory, _)| path.starts_with(directory))
                .max_by_key(|(directory, _)| directory.components().count());
            if let Some((_, id)) = member {
                return Owner::Member(id.clone());
            }
        }
        Owner::NonMember(NonMemberOwner::Path(path))
    }
}

impl Diagnostic {
    pub fn new(level: Level, code: Option<String>, message: String) -> Self {
        Self {
            level,
            code,
            message,
            site: None,
            detail: None,
            package_id: None,
            owner: Owner::Unknown,
        }
    }

    pub fn is_error(&self) -> bool {
        self.level == Level::Error
    }

    pub fn is_warning(&self) -> bool {
        self.level == Level::Warning
    }

    /// Whether this warning belongs to a package outside the metadata
    /// workspace. Errors still fail regardless of ownership.
    pub fn is_dependency_warning(&self) -> bool {
        self.is_warning() && self.owner.is_non_member()
    }

    pub fn path(&self) -> Option<&Path> {
        self.site.as_ref().map(|site| site.file.as_path())
    }

    pub fn location(&self) -> Option<String> {
        let site = self.site.as_ref()?;
        match (site.line, site.column) {
            (Some(line), Some(column)) => Some(format!("{}:{line}:{column}", site.file.display())),
            _ => Some(site.file.display().to_string()),
        }
    }

    pub fn manifest_path(&self) -> Option<PathBuf> {
        if let Some(site) = &self.site
            && site
                .file
                .file_name()
                .is_some_and(|name| name == "Cargo.toml")
        {
            return Some(site.file.clone());
        }

        if !self.is_warning() || self.site.is_some() {
            return None;
        }
        let (path, _) = self.message.split_once(": ")?;
        let path = Path::new(path);
        path.file_name()
            .is_some_and(|name| name == "Cargo.toml")
            .then(|| path.to_owned())
    }

    pub fn classify_owner(&mut self, owners: &OwnerIndex) {
        self.owner = owners.classify(self);
    }

    pub fn format_one(&self) -> String {
        self.format_as(self.level)
    }

    /// Render using a chosen severity while keeping code, site and message
    /// sourced from the structured diagnostic.
    pub fn format_as(&self, level: Level) -> String {
        let header = match (level, self.code.as_deref()) {
            (Level::Error, Some(code)) => format!("error[{code}]"),
            (Level::Warning, Some(code)) => format!("warning[{code}]"),
            (Level::Other, Some(code)) => format!("diagnostic[{code}]"),
            (Level::Error, None) => "error".to_owned(),
            (Level::Warning, None) => "warning".to_owned(),
            (Level::Other, None) => "diagnostic".to_owned(),
        };
        let base = match self.location() {
            Some(location) => format!("{header} {location} {}", self.message),
            None => format!("{header} {}", self.message),
        };
        match &self.detail {
            Some(detail) => format!("{base} - {detail}"),
            None => base,
        }
    }

    pub fn key_as(&self, level: Level) -> DiagnosticKey {
        DiagnosticKey {
            level,
            code: self.code.clone(),
            site: self.site.clone(),
            message: self.message.clone(),
        }
    }
}

/// Stable dedupe identity derived from diagnostic fields rather than rendered
/// header/location strings.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiagnosticKey {
    pub level: Level,
    pub code: Option<String>,
    pub site: Option<Site>,
    pub message: String,
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}
