/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use configparser::config::ConfigSet;

use crate::constants::*;
use crate::errors::InitError;

pub fn init_hg_repo(
    root_path: &Path,
    config: &ConfigSet,
    hgrc_contents: Option<String>,
) -> Result<(), InitError> {
    if !root_path.exists() {
        create_dir(root_path)?;
    }
    let hg_path = root_path.join(HG_PATH);
    let hg_path = hg_path.as_path();
    if hg_path.exists() {
        return Err(InitError::ExistingRepoError(PathBuf::from(root_path)));
    }
    create_dir(hg_path)?;

    write_reponame(hg_path, config)?;
    write_changelog(hg_path)?;
    write_hgrc(hg_path, hgrc_contents)?;
    write_requirements(hg_path)?;
    write_store_requirements(hg_path, config)?;
    // TODO(sggutier): Add cleanup for the .hg directory in the event of an error

    Ok(())
}

fn create_dir(path: &Path) -> Result<(), InitError> {
    match fs::create_dir_all(path) {
        Err(err) => Err(InitError::DirectoryCreationError(
            path.to_str().unwrap().to_string(),
            err,
        )),
        _ => Ok(()),
    }
}

fn create_file(path: &Path, contents: &[u8]) -> Result<(), InitError> {
    let mut file = match File::create(path) {
        Ok(file) => file,
        Err(err) => {
            return Err(InitError::FileCreationError(PathBuf::from(path), err));
        }
    };
    match file.write_all(contents) {
        Ok(_) => Ok(()),
        Err(err) => {
            fs::remove_file(path).ok();
            Err(InitError::FileCreationError(PathBuf::from(path), err))
        }
    }
}

fn write_reponame<T: AsRef<Path>>(path: T, config: &ConfigSet) -> Result<(), InitError> {
    let path = path.as_ref();
    if let Some(reponame) = config.get("remotefilelog", "reponame") {
        let reponame_path = path.join(REPONAME_FILE);
        if !reponame_path.exists() {
            create_file(reponame_path.as_path(), reponame.as_bytes())?;
        }
    }
    Ok(())
}

// TODO(sggutier): We want to avoid creating this file in the first place
fn write_changelog(path: &Path) -> Result<(), InitError> {
    let changelog_path = path.join(CHANGELOG_FILE);
    create_file(
        changelog_path.as_path(),
        b"\0\0\01 dummy changelog to prevent using the old repo layout",
    )
}

fn write_hgrc(path: &Path, hgrc_contents: Option<String>) -> Result<(), InitError> {
    if let Some(hgrc_contents) = hgrc_contents {
        let hgrc_path = path.join(HGRC_FILE);
        create_file(hgrc_path.as_path(), hgrc_contents.as_bytes())?;
    };
    Ok(())
}

fn write_requirements_file(path: &Path, requirements: HashSet<&str>) -> Result<(), InitError> {
    let mut requirements: Vec<_> = requirements.into_iter().collect();
    requirements.sort_unstable();
    requirements.push("");
    let requirements_path = path.join(REQUIREMENTS_FILE);
    create_file(
        requirements_path.as_path(),
        requirements.join("\n").as_bytes(),
    )
}

fn write_requirements(path: &Path) -> Result<(), InitError> {
    let requirements = HashSet::from([
        "lz4revlog",
        "revlogv1",
        "store",
        "fncache",
        "dotencode",
        "treestate",
        "generaldelta",
    ]);

    write_requirements_file(path, requirements)
}

fn write_store_requirements(path: &Path, config: &ConfigSet) -> Result<(), InitError> {
    let store_path = path.join(STORE_PATH);
    let store_path = store_path.as_path();
    create_dir(store_path)?;
    let mut requirements = HashSet::from(["visibleheads"]);
    if config
        .get_or("format", "use-segmented-changelog", || false)
        .unwrap_or(false)
    {
        requirements.insert("invalidatelinkrev");
        requirements.insert("segmentedchangelog");
    }

    if config
        .get_or("experimental", "narrow-heads", || true)
        .unwrap_or(true)
    {
        requirements.insert("narrowheads");
    }

    write_requirements_file(store_path, requirements)
}

#[cfg(test)]
mod tests {
    use configparser::config::Options;

    use super::*;

    #[test]
    fn test_reponame() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = ConfigSet::new();
        write_reponame(tmp.path(), &config).unwrap();
        let reponame_path = tmp.path().join(REPONAME_FILE);
        let reponame_path = reponame_path.as_path();
        assert!(!reponame_path.exists());
        config.set(
            "remotefilelog",
            "reponame",
            Some("thename"),
            &Options::new(),
        );
        write_reponame(tmp.path(), &config).unwrap();
        assert_eq!(fs::read_to_string(reponame_path).unwrap(), "thename");

        config.set(
            "remotefilelog",
            "reponame",
            Some("newname"),
            &Options::new(),
        );
        write_reponame(tmp.path(), &config).unwrap();
        // Reponame should not be written into if it already exists
        assert_eq!(fs::read_to_string(reponame_path).unwrap(), "thename");
    }

    #[test]
    fn test_requirements() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(REQUIREMENTS_FILE);
        let path = path.as_path();

        write_requirements(tmp.path()).unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            r#"dotencode
fncache
generaldelta
lz4revlog
revlogv1
store
treestate
"#
        );
    }

    #[test]
    fn test_store_requirements() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = ConfigSet::new();
        let options = Options::new();
        let storepath = tmp.path().join(STORE_PATH);
        let path = storepath.join(REQUIREMENTS_FILE);
        let path = path.as_path();
        let mut expected = vec!["narrowheads", "visibleheads", ""];

        write_store_requirements(tmp.path(), &config).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), expected.join("\n"));
        fs::remove_dir_all(storepath.as_path()).unwrap();

        expected.insert(0, "invalidatelinkrev");
        expected.insert(2, "segmentedchangelog");
        config.set("format", "use-segmented-changelog", Some("true"), &options);
        write_store_requirements(tmp.path(), &config).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), expected.join("\n"));
        fs::remove_dir_all(storepath.as_path()).unwrap();

        config.set("experimental", "narrow-heads", Some("no"), &options);
        expected.remove(1);
        write_store_requirements(tmp.path(), &config).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), expected.join("\n"));
    }

    #[test]
    fn test_init_hg_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("somerepo");
        let hg_path = repo_path.join(".hg");

        init_hg_repo(repo_path.as_path(), &mut ConfigSet::new(), None).unwrap();
        assert!(repo_path.exists());
        assert!(hg_path.exists());
        assert!(hg_path.join(CHANGELOG_FILE).exists());
        assert!(hg_path.join(REQUIREMENTS_FILE).exists());
        assert!(hg_path.join(STORE_PATH).exists());
        assert!(hg_path.join(STORE_PATH).join(REQUIREMENTS_FILE).exists());

        fs::remove_dir_all(repo_path.as_path()).unwrap();
        create_dir(repo_path.as_path()).unwrap();
        init_hg_repo(repo_path.as_path(), &mut ConfigSet::new(), None).unwrap();

        fs::remove_dir_all(repo_path.as_path()).unwrap();
        create_dir(repo_path.as_path()).unwrap();
        create_dir(hg_path.as_path()).unwrap();
        let error_str = format!(
            "repository `{}` already exists",
            repo_path.to_str().unwrap()
        );
        let err = init_hg_repo(repo_path.as_path(), &mut ConfigSet::new(), None)
            .err()
            .unwrap();
        assert!(matches!(err, InitError::ExistingRepoError(_)));
        assert_eq!(err.to_string(), error_str);
    }

    #[test]
    fn test_directory_creation() {
        let tmpdir = tempfile::tempdir().unwrap();

        // Test recursive directory creation
        let new_dir_path = tmpdir.path().join("some").join("nested").join("directory");
        create_dir(new_dir_path.as_path()).unwrap();
        init_hg_repo(new_dir_path.as_path(), &mut ConfigSet::new(), None).unwrap();

        // Test getting an error when unable to create directory
        let new_dir_path = tmpdir.path().join("foo");
        File::create(new_dir_path.as_path()).unwrap();
        let err = init_hg_repo(new_dir_path.as_path(), &mut ConfigSet::new(), None)
            .err()
            .unwrap();
        let partial_error_str =
            format!("unable to create directory at `{}", new_dir_path.display(),);
        assert!(matches!(err, InitError::DirectoryCreationError(_, _)));
        assert_eq!(
            err.to_string()[..partial_error_str.len()],
            partial_error_str
        );
    }
}
