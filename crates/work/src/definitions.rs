//! Definitions on disk. A file is a definition's one home: the store holds a
//! copy that says where it came from, and a file that's gone removes its copy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use schema::bounded;
use serde_json::Value;
use store::{
    Definition, DefinitionKey, DefinitionKind, PageRequest, Store, StoreError, UnitOfWork, WriteOp,
};

/// Marks a definition as coming from a file.
const FILE_SOURCE: &str = "file:";
const PAGE_SIZE: u32 = 200;

/// A definition that can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DefinitionError {
    /// A cron expression isn't valid.
    #[error("{name}: not a valid cron expression: {problem}")]
    Cron {
        /// The definition.
        name: String,
        /// What's wrong.
        problem: String,
    },
    /// A time zone isn't known.
    #[error("{name}: unknown time zone {zone}")]
    Zone {
        /// The definition.
        name: String,
        /// The zone.
        zone: String,
    },
    /// It names a kind of job no handler runs.
    #[error("{name}: no handler for job kind {kind}")]
    UnknownKind {
        /// The definition.
        name: String,
        /// The kind.
        kind: String,
    },
    /// It doesn't meet its schema, or refers to something that isn't there.
    #[error("{name}: {problem}")]
    Invalid {
        /// The definition.
        name: String,
        /// What's wrong.
        problem: String,
    },
    /// The same name already exists from somewhere else.
    #[error("{name} comes from {source_name}, so {file} can't define it too")]
    TwoHomes {
        /// The definition.
        name: String,
        /// Where the existing one came from.
        source_name: String,
        /// The file.
        file: String,
    },
    /// A file couldn't be read.
    #[error("{0}")]
    File(String),
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// One definition, read from disk and not yet checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionFile {
    /// What it defines.
    pub kind: DefinitionKind,
    /// Its name, which is the file's.
    pub name: String,
    /// Where it is.
    pub path: PathBuf,
    /// Its path under the definitions root.
    pub relative: String,
    /// What it says.
    pub body: Value,
}

fn file_problem(path: &Path, problem: impl std::fmt::Display) -> DefinitionError {
    DefinitionError::File(bounded(&format!("{}: {problem}", path.display())))
}

/// Whether `name` can name a definition: 1 to 64 lowercase letters,
/// digits, `_` and `-`, so it's also a plain file name.
#[must_use]
pub fn is_definition_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn name_of(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    is_definition_name(stem).then(|| stem.to_owned())
}

/// Reads every `*.yaml` under each `(kind, directory)` of `root`. A missing
/// directory is fine.
///
/// # Errors
///
/// [`DefinitionError::File`] naming the file that can't be read.
pub fn read_definition_files(
    root: &Path,
    directories: &[(DefinitionKind, &str)],
) -> Result<Vec<DefinitionFile>, DefinitionError> {
    let mut files = Vec::new();
    for (kind, directory) in directories {
        let folder = root.join(directory);
        if !folder.is_dir() {
            continue;
        }
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(&folder).map_err(|e| file_problem(&folder, e))? {
            paths.push(entry.map_err(|e| file_problem(&folder, e))?.path());
        }
        paths.sort();
        for path in paths {
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let name = name_of(&path).ok_or_else(|| {
                file_problem(&path, "a name is lowercase letters, digits, - and _")
            })?;
            let text = std::fs::read_to_string(&path).map_err(|e| file_problem(&path, e))?;
            let body = serde_saphyr::from_str(&text).map_err(|e| file_problem(&path, e))?;
            files.push(DefinitionFile {
                kind: *kind,
                relative: format!("{directory}/{name}.yaml"),
                name,
                path,
                body,
            });
        }
    }
    Ok(files)
}

/// Every definition of a kind.
///
/// # Errors
///
/// [`StoreError`] if they can't be read.
pub async fn list_definitions(
    store: &dyn Store,
    kind: DefinitionKind,
) -> Result<Vec<Definition>, StoreError> {
    let mut found = Vec::new();
    let mut offset = 0;
    loop {
        let page = store
            .list_definitions(kind, PageRequest::new(PAGE_SIZE, offset)?)
            .await?;
        let got = u32::try_from(page.items.len()).unwrap_or(u32::MAX);
        found.extend(page.items);
        if !page.has_more || got == 0 {
            return Ok(found);
        }
        offset = offset.saturating_add(got);
    }
}

/// Makes the store's file-backed definitions of `kinds` match `files`, all
/// at once. Call it only once every file has been checked.
///
/// # Errors
///
/// [`DefinitionError::TwoHomes`] if a file names a definition that came from
/// somewhere else, or [`StoreError`] if the store fails.
pub async fn save_definitions(
    store: &dyn Store,
    files: &[DefinitionFile],
    kinds: &[DefinitionKind],
    now: DateTime<Utc>,
) -> Result<usize, DefinitionError> {
    let present: BTreeMap<(DefinitionKind, &str), &str> = files
        .iter()
        .map(|file| ((file.kind, file.name.as_str()), file.relative.as_str()))
        .collect();
    let mut unit = UnitOfWork::new();
    for kind in kinds {
        for existing in list_definitions(store, *kind).await? {
            let from_file = existing.source.starts_with(FILE_SOURCE);
            let file = present.get(&(*kind, existing.key.name.as_str()));
            if let (Some(file), false) = (file, from_file) {
                return Err(DefinitionError::TwoHomes {
                    name: existing.key.name.clone(),
                    source_name: existing.source,
                    file: (*file).to_owned(),
                });
            }
            if from_file && file.is_none() {
                unit = unit.with(WriteOp::DeleteDefinition(existing.key));
            }
        }
    }
    for file in files {
        unit = unit.with(WriteOp::PutDefinition(Definition {
            key: DefinitionKey {
                kind: file.kind,
                name: file.name.clone(),
            },
            body: file.body.clone(),
            source: format!("{FILE_SOURCE}{}", file.relative),
            updated_at: now,
        }));
    }
    store.apply(unit).await?;
    Ok(files.len())
}
