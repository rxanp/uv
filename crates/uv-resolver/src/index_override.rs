//! Download locked registry distributions from indexes provided on the command line.
//!
//! The lockfile records the download URL of every registry wheel and source distribution, so
//! installing from a lockfile (e.g., `uv sync --frozen`) would otherwise ignore indexes passed via
//! `--index`, `--default-index`, `--index-url`, `--extra-index-url`, or their environment
//! variables (e.g., `UV_INDEX`). This lets users install a lockfile from a mirror without
//! rewriting it.
//!
//! Each locked file is looked up by filename on the command-line indexes, in order. The first
//! index that lists the file is used to download it. Files that aren't listed on any of those
//! indexes are downloaded from the URL in the lockfile, so a mirror that only carries a subset of
//! the lockfile's packages is still usable.
//!
//! The lockfile remains the source of truth for integrity: every artifact is verified against the
//! hashes recorded in the lockfile, regardless of where it was downloaded from. A file whose
//! hashes, as advertised by the index, disagree with the lockfile is not downloaded from that
//! index.

use std::sync::Arc;

use futures::StreamExt;
use rustc_hash::FxHashMap;
use tokio::sync::Semaphore;
use tracing::debug;

use uv_cache::Cache;
use uv_client::{
    BaseClientBuilder, ClientBuildError, MetadataFormat, RegistryClient, RegistryClientBuilder,
    VersionFiles,
};
use uv_configuration::{Concurrency, IndexStrategy};
use uv_distribution_filename::DistFilename;
use uv_distribution_types::{
    BuiltDist, Dist, File, Index, IndexCapabilities, IndexLocations, IndexMetadataRef, IndexUrl,
    Origin, RegistryBuiltWheel, RegistrySourceDist, Resolution, ResolvedDist, SourceDist,
};
use uv_normalize::PackageName;
use uv_pypi_types::HashDigests;
use uv_warnings::warn_user;

/// A registry file in the lockfile that may be downloaded from a command-line index.
#[derive(Debug)]
struct LockedFile {
    /// The filename as recorded in the lockfile.
    filename: String,
    /// The parsed filename, used to match files that an index lists under a non-normalized name
    /// (e.g., `SCons-4.8.1-py3-none-any.whl` for `scons-4.8.1-py3-none-any.whl`).
    dist_filename: Option<DistFilename>,
    hashes: HashDigests,
}

/// A file listed by a command-line index, to download in place of the lockfile URL.
#[derive(Debug)]
struct Replacement {
    file: File,
    index: IndexUrl,
}

/// Rewrite the registry distributions in a [`Resolution`] read from a lockfile to download from
/// the indexes provided on the command line, where those indexes list the locked files.
pub async fn apply_index_overrides(
    resolution: Resolution,
    index_locations: &IndexLocations,
    client_builder: &BaseClientBuilder<'_>,
    cache: &Cache,
    concurrency: &Concurrency,
) -> Result<Resolution, ClientBuildError> {
    let overrides = index_locations
        .simple_indexes()
        .filter(|index| index.origin == Some(Origin::Cli))
        .cloned()
        .collect::<Vec<_>>();
    if overrides.is_empty() {
        return Ok(resolution);
    }

    let locked_files = locked_files(&resolution, &overrides);
    if locked_files.is_empty() {
        return Ok(resolution);
    }

    debug!(
        "Looking up {} locked package(s) on command-line indexes",
        locked_files.len()
    );

    // Constructing the client caches any credentials attached to the command-line indexes, which
    // are reused when the files are downloaded.
    let client = RegistryClientBuilder::new(client_builder.clone(), cache.clone())
        .index_locations(IndexLocations::new(overrides.clone(), Vec::new(), false))
        .index_strategy(IndexStrategy::FirstIndex)
        .build()?;
    let capabilities = IndexCapabilities::default();
    let download_concurrency = Semaphore::new(concurrency.downloads);

    let replacements = futures::stream::iter(locked_files)
        .map(async |(package_name, files)| {
            find_replacements(
                &client,
                &overrides,
                &capabilities,
                &download_concurrency,
                &package_name,
                files,
            )
            .await
        })
        .buffer_unordered(concurrency.downloads)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .flatten()
        .collect::<FxHashMap<String, Replacement>>();

    if replacements.is_empty() {
        return Ok(resolution);
    }
    debug!(
        "Downloading {} locked file(s) from command-line indexes",
        replacements.len()
    );

    Ok(resolution.map(|resolved| match resolved {
        ResolvedDist::Installed { .. } => None,
        ResolvedDist::Installable { dist, version } => {
            let dist = match dist.as_ref() {
                Dist::Built(BuiltDist::Registry(built)) => {
                    let mut built = built.clone();
                    let mut changed = false;
                    for wheel in &mut built.wheels {
                        if let Some(replacement) = replacements.get(wheel.file.filename.as_ref()) {
                            *wheel = replace_wheel(wheel, replacement);
                            changed = true;
                        }
                    }
                    if let Some(sdist) = &mut built.sdist
                        && let Some(replacement) = replacements.get(sdist.file.filename.as_ref())
                    {
                        *sdist = replace_sdist(sdist, replacement);
                        changed = true;
                    }
                    if !changed {
                        return None;
                    }
                    Dist::Built(BuiltDist::Registry(built))
                }
                Dist::Source(SourceDist::Registry(sdist)) => {
                    let replacement = replacements.get(sdist.file.filename.as_ref())?;
                    Dist::Source(SourceDist::Registry(replace_sdist(sdist, replacement)))
                }
                Dist::Built(
                    BuiltDist::DirectUrl(_) | BuiltDist::Path(_) | BuiltDist::GitPath(_),
                )
                | Dist::Source(
                    SourceDist::DirectUrl(_)
                    | SourceDist::GitDirectory(_)
                    | SourceDist::GitPath(_)
                    | SourceDist::Path(_)
                    | SourceDist::Directory(_),
                ) => return None,
            };
            Some(ResolvedDist::Installable {
                dist: Arc::new(dist),
                version: version.clone(),
            })
        }
    }))
}

/// Collect the remote registry files in the [`Resolution`], grouped by package.
///
/// Files that are already locked to one of the command-line indexes are omitted, since they are
/// downloaded from that index regardless. Files from local (path-based) registries are omitted,
/// since they're read from disk.
fn locked_files(
    resolution: &Resolution,
    overrides: &[Index],
) -> FxHashMap<PackageName, Vec<LockedFile>> {
    let is_candidate = |index: &IndexUrl| match index {
        IndexUrl::Pypi(_) | IndexUrl::Url(_) => !overrides
            .iter()
            .any(|override_index| override_index.url() == index),
        IndexUrl::Path(_) => false,
    };

    let mut locked_files = FxHashMap::<PackageName, Vec<LockedFile>>::default();
    for resolved in resolution.distributions() {
        let dist = match resolved {
            ResolvedDist::Installed { .. } => continue,
            ResolvedDist::Installable { dist, .. } => dist,
        };
        match dist.as_ref() {
            Dist::Built(BuiltDist::Registry(built)) => {
                for wheel in &built.wheels {
                    if is_candidate(&wheel.index) {
                        locked_files
                            .entry(wheel.filename.name.clone())
                            .or_default()
                            .push(LockedFile::from_wheel(wheel));
                    }
                }
                if let Some(sdist) = &built.sdist
                    && is_candidate(&sdist.index)
                {
                    locked_files
                        .entry(sdist.name.clone())
                        .or_default()
                        .push(LockedFile::from_sdist(sdist));
                }
            }
            Dist::Source(SourceDist::Registry(sdist)) => {
                if is_candidate(&sdist.index) {
                    locked_files
                        .entry(sdist.name.clone())
                        .or_default()
                        .push(LockedFile::from_sdist(sdist));
                }
            }
            Dist::Built(BuiltDist::DirectUrl(_) | BuiltDist::Path(_) | BuiltDist::GitPath(_))
            | Dist::Source(
                SourceDist::DirectUrl(_)
                | SourceDist::GitDirectory(_)
                | SourceDist::GitPath(_)
                | SourceDist::Path(_)
                | SourceDist::Directory(_),
            ) => {}
        }
    }
    locked_files
}

impl LockedFile {
    fn from_wheel(wheel: &RegistryBuiltWheel) -> Self {
        Self {
            filename: wheel.file.filename.to_string(),
            dist_filename: Some(DistFilename::WheelFilename(wheel.filename.clone())),
            hashes: wheel.file.hashes.clone(),
        }
    }

    fn from_sdist(sdist: &RegistrySourceDist) -> Self {
        Self {
            filename: sdist.file.filename.to_string(),
            dist_filename: DistFilename::try_from_filename(&sdist.file.filename, &sdist.name),
            hashes: sdist.file.hashes.clone(),
        }
    }

    /// Returns `true` if the file listed by an index is this locked file.
    fn matches(&self, listed_filename: &DistFilename, listed: &File) -> bool {
        if let Some(dist_filename) = &self.dist_filename {
            dist_filename == listed_filename
        } else {
            self.filename == listed.filename.as_ref()
        }
    }
}

/// Search the command-line indexes, in order, for the locked files of a single package.
///
/// Returns the files that were found, keyed by filename.
async fn find_replacements(
    client: &RegistryClient,
    overrides: &[Index],
    capabilities: &IndexCapabilities,
    download_concurrency: &Semaphore,
    package_name: &PackageName,
    mut remaining: Vec<LockedFile>,
) -> Vec<(String, Replacement)> {
    let mut replacements = Vec::new();

    for index in overrides {
        if remaining.is_empty() {
            break;
        }

        let metadata = match client
            .simple_detail(
                package_name,
                Some(IndexMetadataRef::from(index)),
                capabilities,
                download_concurrency,
            )
            .await
        {
            Ok(metadata) => metadata,
            Err(err) => {
                debug!(
                    "Failed to look up `{package_name}` on {}: {err}",
                    index.url()
                );
                continue;
            }
        };

        let mut listed = Vec::<(DistFilename, File)>::new();
        for (_, format) in metadata {
            match format {
                MetadataFormat::Simple(archive) => {
                    for datum in archive.iter() {
                        match rkyv::deserialize::<VersionFiles, rkyv::rancor::Error>(&datum.files) {
                            Ok(files) => {
                                listed.extend(files.all(package_name));
                            }
                            Err(err) => {
                                debug!(
                                    "Failed to read files for `{package_name}` on {}: {err}",
                                    index.url()
                                );
                            }
                        }
                    }
                }
                MetadataFormat::Flat(entries) => {
                    for entry in entries {
                        let (filename, file, _) = entry.into_parts();
                        listed.push((filename, file));
                    }
                }
            }
        }

        remaining.retain(|locked| {
            let Some(position) = listed
                .iter()
                .position(|(filename, file)| locked.matches(filename, file))
            else {
                return true;
            };
            let (_, file) = listed.swap_remove(position);
            if !hashes_agree(&locked.hashes, &file.hashes) {
                warn_user!(
                    "Ignoring `{}` on {} because its hash does not match the lockfile",
                    locked.filename,
                    index.url()
                );
                return true;
            }
            replacements.push((
                locked.filename.clone(),
                Replacement {
                    file,
                    index: index.url().clone(),
                },
            ));
            false
        });
    }

    for locked in &remaining {
        debug!(
            "`{}` is not available on any command-line index; using the lockfile URL",
            locked.filename
        );
    }

    replacements
}

/// Returns `true` unless the index advertises a digest that differs from the lockfile for an
/// algorithm recorded in both.
fn hashes_agree(locked: &HashDigests, advertised: &HashDigests) -> bool {
    locked.iter().all(|locked| {
        advertised
            .iter()
            .filter(|advertised| advertised.algorithm() == locked.algorithm())
            .all(|advertised| advertised == locked)
    })
}

/// Download the wheel from the command-line index, keeping the lockfile's metadata (including
/// hashes), since the lockfile remains the source of truth for the file's contents.
fn replace_wheel(wheel: &RegistryBuiltWheel, replacement: &Replacement) -> RegistryBuiltWheel {
    RegistryBuiltWheel {
        filename: wheel.filename.clone(),
        file: Box::new(replace_file(&wheel.file, &replacement.file)),
        index: replacement.index.clone(),
        size_is_authoritative: wheel.size_is_authoritative,
    }
}

/// Download the source distribution from the command-line index, keeping the lockfile's metadata
/// (including hashes).
fn replace_sdist(sdist: &RegistrySourceDist, replacement: &Replacement) -> RegistrySourceDist {
    RegistrySourceDist {
        file: Box::new(replace_file(&sdist.file, &replacement.file)),
        index: replacement.index.clone(),
        ..sdist.clone()
    }
}

fn replace_file(locked: &File, listed: &File) -> File {
    File {
        url: listed.url.clone(),
        ..locked.clone()
    }
}
