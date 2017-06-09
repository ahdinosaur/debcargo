use cargo;
use cargo::core::{Dependency, Source, SourceId, PackageId, Summary, Registry, TargetKind};
use cargo::util::FileLock;
use cargo::core::{manifest, package};
use semver::Version;
use itertools::Itertools;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use tar::Archive;
use tempdir::TempDir;

use std;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::io;
use std::fs;

use errors::*;
use debian::deb_dep;

pub struct CrateInfo {
    package: package::Package,
    manifest: manifest::Manifest,
    summary: Summary,
    crate_file: FileLock,
}

fn hash<H: Hash>(hashable: &H) -> u64 {
    #![allow(deprecated)]
    let mut hasher = std::hash::SipHasher::new();
    hashable.hash(&mut hasher);
    hasher.finish()
}

impl CrateInfo {
    pub fn new(crate_name: &str, version: Option<&str>) -> Result<CrateInfo> {
        let version = version.map(|v| if v.starts_with(|c: char| c.is_digit(10)) {
            ["=", v].concat()
        } else {
            v.to_string()
        });
        let config = cargo::Config::default()?;
        let crates_io = SourceId::crates_io(&config)?;
        let mut registry = cargo::sources::RegistrySource::remote(&crates_io, &config);
        let dependency = Dependency::parse_no_deprecated(&crate_name,
                                                         version.as_ref().map(String::as_str),
                                                         &crates_io)?;
        let summaries = registry.query(&dependency)?;
        let registry_name = format!("{}-{:016x}",
                                    crates_io.url().host_str().unwrap_or(""),
                                    hash(&crates_io).swap_bytes());




        let summary = summaries.iter()
            .max_by_key(|s| s.package_id())
            .ok_or_else(|| {
                format!("Couldn't find any crate matching {} {}\n Try `debcargo cargo-update` to \
                         update the crates.io index",
                        dependency.name(),
                        dependency.version_req())
            })?;

        let pkgid = summary.package_id();
        let package = registry.download(&pkgid)?;
        let manifest = package.manifest();
        let filename = format!("{}-{}.crate", pkgid.name(), pkgid.version());
        let crate_file = config.registry_cache_path()
            .join(&registry_name)
            .open_ro(&filename, &config, &filename)?;

        Ok(CrateInfo {
            package: package.clone(),
            manifest: manifest.clone(),
            summary: summary.clone(),
            crate_file: crate_file,
        })

    }

    pub fn targets(&self) -> &[manifest::Target] {
        self.manifest.targets()
    }

    pub fn version(&self) -> &Version {
        self.summary.package_id().version()
    }

    pub fn manifest(&self) -> &manifest::Manifest {
        &self.manifest
    }

    pub fn features(&self) -> &HashMap<String, Vec<String>> {
        self.summary.features()
    }

    pub fn checksum(&self) -> Option<&str> {
        self.summary.checksum()
    }

    pub fn package_id(&self) -> &PackageId {
        self.summary.package_id()
    }

    pub fn metadata(&self) -> &manifest::ManifestMetadata {
        self.manifest.metadata()
    }

    pub fn summary(&self) -> &Summary {
        &self.summary
    }

    pub fn package(&self) -> &package::Package {
        &self.package
    }

    pub fn crate_file(&self) -> &FileLock {
        &self.crate_file
    }

    pub fn dependencies(&self) -> &[Dependency] {
        self.manifest.dependencies()
    }

    pub fn default_deps_features(&self) -> Option<(HashSet<&str>, HashSet<&str>)> {
        let mut default_features = HashSet::new();
        let mut default_deps = HashSet::new();

        let mut defaults = Vec::new();
        let features = self.summary.features();

        defaults.push("default");
        default_features.insert("default");

        while let Some(feature) = defaults.pop() {
            match features.get(feature) {
                Some(l) => {
                    default_features.insert(feature);
                    for f in l {
                        defaults.push(f);
                    }
                }
                None => {
                    default_deps.insert(feature);
                }
            }
        }

        for (feature, deps) in features {
            if deps.is_empty() {
                default_features.insert(feature.as_str());
            }
        }

        Some((default_features, default_deps))
    }

    pub fn non_default_features(&self, default_features: &HashSet<&str>) -> Option<Vec<&str>> {
        let features = self.summary.features();
        Some(features.keys().map(String::as_str).filter(|f| !default_features.contains(f)).sorted())
    }

    pub fn is_lib(&self) -> bool {
        let mut lib = false;
        for target in self.manifest.targets() {
            match target.kind() {
                &TargetKind::Lib(_) => {
                    lib = true;
                    break;
                }
                _ => continue,
            }
        }

        lib
    }

    pub fn get_binary_targets(&self) -> Vec<&str> {
        let mut bins = Vec::new();
        for target in self.manifest.targets() {
            match target.kind() {
                &TargetKind::Bin => {
                    bins.push(target.name());
                }
                _ => continue,
            }
        }
        bins.sort();
        bins
    }

    pub fn version_suffix(&self) -> String {
        let lib = self.is_lib();
        let bins = self.get_binary_targets();

        let version_suffix = match self.package_id().version() {
            _ if !lib && !bins.is_empty() => "".to_string(),
            &Version { major: 0, minor, .. } => format!("-0.{}", minor),
            &Version { major, .. } => format!("-{}", major),
        };

        version_suffix

    }

    pub fn dev_dependencies(&self) -> HashSet<&str> {
        use cargo::core::dependency::Kind;
        let mut dev_deps = HashSet::new();
        for dep in self.dependencies().iter() {
            if dep.kind() == Kind::Development {
                dev_deps.insert(dep.name());
            }
        }

        dev_deps
    }

    pub fn non_build_dependencies(&self) -> Result<HashMap<&str, &Dependency>> {
        let mut all_deps = HashMap::new();
        let dev_deps = self.dev_dependencies();
        for dep in self.dependencies().iter() {
            if !dep.is_build() && !dev_deps.contains(dep.name()) {
                if all_deps.insert(dep.name(), dep).is_some() {
                    bail!("Duplicate dependency for {}", dep.name());
                }
            }
        }

        Ok(all_deps)
    }

    pub fn non_dev_dependencies(&self) -> Result<Vec<String>> {
        let (_, default_deps) = self.default_deps_features().unwrap();
        let dev_deps = self.dev_dependencies();
        let mut deps = Vec::new();

        for dep in self.dependencies().iter() {
            if !dev_deps.contains(dep.name()) &&
               (!dep.is_optional() || default_deps.contains(dep.name())) {
                deps.push(try!(deb_dep(dep)));
            }
        }

        deps.sort();
        deps.dedup();
        Ok(deps)
    }

    pub fn get_summary_description(&self) -> (Option<String>, Option<String>) {
        let (summary, description) = if let Some(ref description) = self.metadata().description {
            let mut description = description.trim();
            for article in ["a ", "A ", "an ", "An ", "the ", "The "].iter() {
                description = description.trim_left_matches(article);
            }

            let p1 = description.find('\n');
            let p2 = description.find(". ");
            match p1.into_iter().chain(p2.into_iter()).min() {
                Some(p) => {
                    let s = description[..p].trim_right_matches('.').to_string();
                    let d = description[p + 1..].trim();
                    if d.is_empty() {
                        (Some(s), None)
                    } else {
                        (Some(s), Some(d.to_string()))
                    }
                }
                None => (Some(description.trim_right_matches('.').to_string()), None),
            }
        } else {
            (None, None)
        };

        (summary, description)
    }

    pub fn get_feature_dependencies<F>(&self,
                                       feature: &str,
                                       deb_feature: &F,
                                       feature_deps: &mut Vec<String>)
                                       -> Result<()>
        where F: Fn(&str) -> String
    {
        let (default_features, _) = self.default_deps_features().unwrap();
        let dev_deps = self.dev_dependencies();
        let all_deps = self.non_build_dependencies()?;


        // Track the (possibly empty) additional features required for each dep, to call
        // deb_dep once for all of them.
        let mut deps_features = HashMap::new();
        let features = self.summary().features();
        for dep_str in features.get(feature).unwrap() {
            let mut dep_tokens = dep_str.splitn(2, '/');
            let dep_name = dep_tokens.next().unwrap();
            match dep_tokens.next() {
                None if features.contains_key(dep_name) => {
                    if !default_features.contains(dep_name) {
                        feature_deps
                            .push(format!("{} (= ${{binary:Version}})", deb_feature(dep_name)));
                    }
                }
                opt_dep_feature => {
                    deps_features.entry(dep_name)
                        .or_insert(vec![])
                        .extend(opt_dep_feature.into_iter()
                            .map(String::from));
                }
            }
        }
        for (dep_name, dep_features) in deps_features.into_iter().sorted() {
            if let Some(&dep_dependency) = all_deps.get(dep_name) {
                if dep_features.is_empty() {
                    feature_deps.push(try!(deb_dep(dep_dependency)));
                } else {
                    let inner = dep_dependency.clone_inner().set_features(dep_features);
                    feature_deps.push(try!(deb_dep(&inner.into_dependency())));
                }
            } else if dev_deps.contains(dep_name) {
                continue;
            } else {
                bail!("Feature {} depended on non-existent dep {}",
                      feature,
                      dep_name);
            };
        }

        Ok(())
    }

    pub fn extract_crate(&self, path: &Path) -> Result<bool> {
        let mut archive = Archive::new(GzDecoder::new(self.crate_file.file())?);
        let tempdir = TempDir::new_in(".", "debcargo")?;
        let mut source_modified = false;

        // Filter out static libraries, to avoid needing to patch all the winapi crates to remove
        // import libraries.
        let remove_path = |path: &Path| match path.extension() {
            Some(ext) if ext == "a" => true,
            _ => false,
        };

        for entry in archive.entries()? {
            let mut entry = entry?;
            if remove_path(&(entry.path()?)) {
                source_modified = true;
                continue;
            }

            if !entry.unpack_in(tempdir.path())? {
                bail!("Crate contained path traversals via '..'");
            }
        }

        let entries = tempdir.path().read_dir()?.collect::<io::Result<Vec<_>>>()?;
        if entries.len() != 1 || !entries[0].file_type()?.is_dir() {
            let pkgid = self.package_id();
            bail!("{}-{}.crate did not unpack to a single top-level directory",
                  pkgid.name(),
                  pkgid.version());
        }

        if let Err(e) = fs::rename(entries[0].path(), &path) {
            Err(e).chain_err(|| {
                    format!("Could not create source directory {0}\n
           To regenerate, \
                             move or remove {0}",
                            path.display())
                })?;
        }

        Ok(source_modified)
    }
}