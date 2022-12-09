use anyhow::{bail, Context};
use std::{ops::Deref, sync::Arc};

use tracing::*;
#[allow(unused_imports)]
use tracing::{error, warn};
use webc::{Annotation, FsEntryType, UrlOrManifest, WebC};

use crate::{
    bin_factory::{BinaryPackage, BinaryPackageCommand},
    VirtualTaskManager, WasiRuntimeImplementation,
};

#[cfg(feature = "wapm-tar")]
mod manifest;
mod pirita;

use crate::http::{DynHttpClient, HttpRequest, HttpRequestOptions};
use pirita::*;

pub(crate) fn fetch_webc_task(
    cache_dir: &str,
    webc: &str,
    runtime: &dyn WasiRuntimeImplementation,
    tasks: &dyn VirtualTaskManager,
) -> Result<BinaryPackage, anyhow::Error> {
    let client = runtime
        .http_client()
        .context("no http client available")?
        .clone();

    let f = {
        let cache_dir = cache_dir.to_string();
        let webc = webc.to_string();
        async move { fetch_webc(&cache_dir, &webc, client).await }
    };

    let result = tasks.block_on(f).context("webc fetch task has died");
    result.with_context(|| format!("could not fetch webc '{webc}'"))
}

async fn fetch_webc(
    cache_dir: &str,
    webc: &str,
    client: DynHttpClient,
) -> Result<BinaryPackage, anyhow::Error> {
    let name = webc.split_once(":").map(|a| a.0).unwrap_or_else(|| webc);
    let (name, version) = match name.split_once("@") {
        Some((name, version)) => (name, Some(version)),
        None => (name, None),
    };
    let url_query = match version {
        Some(version) => WAPM_WEBC_QUERY_SPECIFIC
            .replace(WAPM_WEBC_QUERY_TAG, name.replace("\"", "'").as_str())
            .replace(WAPM_WEBC_VERSION_TAG, version.replace("\"", "'").as_str()),
        None => WAPM_WEBC_QUERY_LAST.replace(WAPM_WEBC_QUERY_TAG, name.replace("\"", "'").as_str()),
    };
    let url = format!(
        "{}{}",
        WAPM_WEBC_URL,
        urlencoding::encode(url_query.as_str())
    );

    let response = client
        .request(HttpRequest {
            url,
            method: "GET".to_string(),
            headers: vec![],
            body: None,
            options: HttpRequestOptions::default(),
        })
        .await?;

    if response.status != 200 {
        bail!(" http request failed with status {}", response.status);
    }
    let body = response.body.context("HTTP response with empty body")?;
    let data: WapmWebQuery =
        serde_json::from_slice(&body).context("Could not parse webc registry JSON data")?;
    let PiritaVersionedDownload {
        url: download_url,
        version,
    } = wapm_extract_version(&data).context("No pirita download URL available")?;
    let mut pkg = download_webc(cache_dir, name, download_url, client).await?;
    pkg.version = version.into();
    Ok(pkg)
}

struct PiritaVersionedDownload {
    url: String,
    version: String,
}

fn wapm_extract_version(data: &WapmWebQuery) -> Option<PiritaVersionedDownload> {
    if let Some(package) = &data.data.get_package_version {
        let url = package.distribution.pirita_download_url.clone()?;
        Some(PiritaVersionedDownload {
            url,
            version: package.version.clone(),
        })
    } else if let Some(package) = &data.data.get_package {
        let url = package
            .last_version
            .distribution
            .pirita_download_url
            .clone()?;
        Some(PiritaVersionedDownload {
            url,
            version: package.last_version.version.clone(),
        })
    } else {
        None
    }
}

async fn download_webc(
    cache_dir: &str,
    name: &str,
    pirita_download_url: String,
    client: DynHttpClient,
) -> Result<BinaryPackage, anyhow::Error> {
    let mut name_comps = pirita_download_url
        .split("/")
        .collect::<Vec<_>>()
        .into_iter()
        .rev();
    let mut name = name_comps.next().unwrap_or_else(|| name);
    let mut name_store;
    for _ in 0..2 {
        if let Some(prefix) = name_comps.next() {
            name_store = format!("{}_{}", prefix, name);
            name = name_store.as_str();
        }
    }
    let compute_path = |cache_dir: &str, name: &str| {
        let name = name.replace("/", "._.");
        std::path::Path::new(cache_dir).join(format!("{}", name.as_str()).as_str())
    };

    // build the parse options
    let options = webc::ParseOptions::default();

    // fast path
    let path = compute_path(cache_dir, name);

    #[cfg(feature = "sys")]
    if path.exists() {
        match webc::WebCMmap::parse(path.clone(), &options) {
            Ok(webc) => unsafe {
                let webc = Arc::new(webc);
                return parse_webc(webc.as_webc_ref(), webc.clone()).with_context(|| {
                    format!("could not parse webc file at path : '{}'", path.display())
                });
            },
            Err(err) => {
                warn!("failed to parse WebC: {}", err);
            }
        }
    }
    if let Ok(data) = std::fs::read(&path) {
        match webc::WebCOwned::parse(data, &options) {
            Ok(webc) => unsafe {
                let webc = Arc::new(webc);
                return parse_webc(webc.as_webc_ref(), webc.clone())
                    .with_context(|| format!("Could not parse webc at {}", path.display()));
            },
            Err(err) => {
                warn!("failed to parse WebC: {}", err);
            }
        }
    }

    // slow path
    let data = download_package(&pirita_download_url, client)
        .await
        .with_context(|| {
            format!(
                "Could not download webc package from '{}'",
                pirita_download_url
            )
        })?;

    #[cfg(feature = "sys")]
    {
        let cache_dir = cache_dir.to_string();
        let name = name.to_string();
        let path = compute_path(cache_dir.as_str(), name.as_str());
        let _ = std::fs::create_dir_all(path.parent().unwrap().clone());

        let mut temp_path = path.clone();
        let rand_128: u128 = rand::random();
        temp_path = std::path::PathBuf::from(format!(
            "{}.{}.temp",
            temp_path.as_os_str().to_string_lossy(),
            rand_128
        ));

        if let Err(err) = std::fs::write(temp_path.as_path(), &data[..]) {
            debug!(
                "failed to write webc cache file [{}] - {}",
                temp_path.as_path().to_string_lossy(),
                err
            );
        }
        if let Err(err) = std::fs::rename(temp_path.as_path(), path.as_path()) {
            debug!(
                "failed to rename webc cache file [{}] - {}",
                temp_path.as_path().to_string_lossy(),
                err
            );
        }

        match webc::WebCMmap::parse(path.clone(), &options) {
            Ok(webc) => unsafe {
                let webc = Arc::new(webc);
                return parse_webc(webc.as_webc_ref(), webc.clone())
                    .with_context(|| format!("Could not parse webc at path '{}'", path.display()));
            },
            Err(err) => {
                warn!("failed to parse WebC: {}", err);
            }
        }
    }

    let webc_raw = webc::WebCOwned::parse(data, &options)
        .with_context(|| format!("Failed to parse downloaded from '{pirita_download_url}'"))?;
    let webc = Arc::new(webc_raw);
    // FIXME: add SAFETY comment
    let package = unsafe {
        parse_webc(webc.as_webc_ref(), webc.clone()).context("Could not parse binary package")?
    };

    Ok(package)
}

async fn download_package(
    download_url: &str,
    client: DynHttpClient,
) -> Result<Vec<u8>, anyhow::Error> {
    let request = HttpRequest {
        url: download_url.to_string(),
        method: "GET".to_string(),
        headers: vec![],
        body: None,
        options: HttpRequestOptions {
            gzip: true,
            cors_proxy: None,
        },
    };
    let response = client.request(request).await?;
    if response.status != 200 {
        bail!("HTTP request failed with status {}", response.status);
    }
    response.body.context("HTTP response with empty body")
}

// TODO: should return Result<_, anyhow::Error>
unsafe fn parse_webc<'a, 'b, T>(webc: webc::WebC<'a>, ownership: Arc<T>) -> Option<BinaryPackage>
where
    T: std::fmt::Debug + Send + Sync + 'static,
    T: Deref<Target = WebC<'static>>,
{
    let package_name = webc.get_package_name();

    let mut pck = webc
        .manifest
        .entrypoint
        .iter()
        .filter_map(|entry| webc.manifest.commands.get(entry).map(|a| (a, entry)))
        .filter_map(|(cmd, entry)| {
            let api = if cmd.runner.starts_with("https://webc.org/runner/emscripten") {
                "emscripten"
            } else if cmd.runner.starts_with("https://webc.org/runner/wasi") {
                "wasi"
            } else {
                warn!("unsupported runner - {}", cmd.runner);
                return None;
            };
            let atom = webc.get_atom_name_for_command(api, entry.as_str());
            match atom {
                Ok(a) => Some(a),
                Err(err) => {
                    warn!(
                        "failed to find atom name for entry command({}) - {} - falling back on the command name itself",
                        entry.as_str(),
                        err
                    );
                    for (name, atom) in webc.manifest.atoms.iter() {
                        tracing::debug!("found atom (name={}, kind={})", name, atom.kind);
                    }
                    Some(entry.clone())
                }
            }
        })
        .filter_map(|atom| match webc.get_atom(&package_name, atom.as_str()) {
            Ok(a) => Some(a),
            Err(err) => {
                warn!("failed to find atom for atom name({}) - {}", atom, err);
                None
            }
        })
        .map(|atom| {
            BinaryPackage::new_with_ownership(
                package_name.as_str(),
                Some(atom.into()),
                ownership.clone(),
            )
        })
        .next();

    // Otherwise add a package without an entry point
    if pck.is_none() {
        pck = Some(BinaryPackage::new_with_ownership(
            package_name.as_str(),
            None,
            ownership.clone(),
        ))
    }
    let mut pck = pck.take().unwrap();

    // Add all the dependencies
    for uses in webc.manifest.use_map.values() {
        let uses = match uses {
            UrlOrManifest::Url(url) => Some(url.path().to_string()),
            UrlOrManifest::Manifest(manifest) => manifest.origin.as_ref().map(|a| a.clone()),
            UrlOrManifest::RegistryDependentUrl(url) => Some(url.clone()),
        };
        if let Some(uses) = uses {
            pck.uses.push(uses);
        }
    }

    // Set the version of this package
    if let Some(Annotation::Map(wapm)) = webc.manifest.package.get("wapm") {
        if let Some(Annotation::Text(version)) = wapm.get(&Annotation::Text("version".to_string()))
        {
            pck.version = version.clone().into();
        }
    } else if let Some(Annotation::Text(version)) = webc.manifest.package.get("version") {
        pck.version = version.clone().into();
    }

    // Add all the file system files
    let top_level_dirs = webc
        .get_volumes_for_package(&package_name)
        .into_iter()
        .flat_map(|volume| {
            webc.volumes
                .get(&volume)
                .unwrap()
                .header
                .top_level
                .iter()
                .filter(|e| e.fs_type == FsEntryType::Dir)
                .map(|e| e.text.to_string())
        })
        .collect::<Vec<_>>();

    pck.webc_fs = Some(Arc::new(wasmer_vfs::webc_fs::WebcFileSystem::init(
        ownership.clone(),
        &package_name,
    )));
    pck.webc_top_level_dirs = top_level_dirs;

    let root_package = webc.get_package_name();
    for (command, action) in webc.get_metadata().commands.iter() {
        let (mut atom, package) = if let Some(Annotation::Map(annotations)) = action.annotations.get("wasi") {
            let mut atom = None;
            let mut package = root_package.clone();
            for (k, v) in annotations {
                match (k, v) {
                    (Annotation::Text(k), Annotation::Text(v)) if k == "atom" => {
                        atom = Some(v.clone());
                    }
                    (Annotation::Text(k), Annotation::Text(v)) if k == "package" => {
                        package = v.clone();
                    }
                    _ => {}
                }
            }
            (atom, package)
        } else {
            (None, root_package.clone())
        };

        // Load the atom as a command
        // TODO... this is a hack until webc is fixed
        if atom.is_none() {
            if webc.get_atom(&package_name, command.as_str()).is_ok() {
                warn!(
                    "failed to find atom name for entry command({}) - falling back on the command name itself",
                    command,
                );
                atom = Some(command.to_string());
            } else if webc.get_atom(&package_name, package_name.as_str()).is_ok() {
                warn!(
                    "failed to find atom name for entry command({}) - falling back on the package name itself",
                    command,
                );
                atom = Some(package_name.to_string());
            } else {
                let first_atom = webc.manifest.atoms.iter().map(|a| a.0.clone()).next();
                if let Some(first_atom) = first_atom {
                    warn!(
                        "failed to find atom name for entry command({}) - falling back on  the package name itself",
                        command,
                    );
                    atom = Some(first_atom);
                } else {
                    warn!(
                        "Failed to match atom to command [{}]",
                        command
                    );
                    for (name, atom) in webc.manifest.atoms.iter() {
                        tracing::debug!("found atom (name={}, kind={})", name, atom.kind);
                    }
                }
            }
        }
        if let Some(atom_name) = atom {
            match webc.get_atom(package.as_str(), atom_name.as_str()) {
                Ok(atom) => {
                    trace!(
                        "added atom (name={}, size={}) for command [{}]",
                        atom_name,
                        atom.len(),
                        command
                    );
                    let mut commands = pck.commands.write().unwrap();
                    commands.push(BinaryPackageCommand::new_with_ownership(
                        command.clone(),
                        atom.into(),
                        ownership.clone(),
                    ));
                }
                Err(err) => {
                    warn!(
                        "Failed to find atom [{}].[{}] - {}",
                        package, atom_name, err
                    );
                }
            }
        }
    }

    Some(pck)
}
