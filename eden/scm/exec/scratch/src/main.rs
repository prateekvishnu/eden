/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! This is "scratch", a tool for locating and creating scratch space.
//! Why not just use the "mktemp" utility?  Scratch creates a persistent
//! and deterministic scratch location for a given input path.  This is
//! useful for holding build artifacts without having them clog up the
//! repository.  In addition, "scratch" is aware that sometimes we
//! may want to use watchman to watch a portion of the scratch space
//! and can arrange the directory structure to prevent over-watching.

#[cfg(unix)]
use anyhow::ensure;
use anyhow::{bail, format_err, Result};
use clap::{App, AppSettings, Arg, SubCommand};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::prelude::*;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// Configuration for scratch space style. This decides whether the directory
/// structure is kept exactly as provided subdir or not.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ScratchStyle {
    /// With flat scratch style, the sub-directories are created one-level under
    /// the repository namespace with serialized names.
    Flat,

    /// With mirror scratch style, the sub-directories mirror the directory
    /// hierarchy of the subdir.
    Mirror,
}

impl Default for ScratchStyle {
    fn default() -> Self {
        ScratchStyle::Flat
    }
}

/// The configuration is intentionally very minimal, and explicitly
/// not made accessible via command line options; the intent is that
/// placement of the scratch space is the policy of the owner of the
/// environment rather than a decision made by the tool that needs
/// the scratch space.
/// Configuration is loaded by parsing the following files as TOML
/// and overlaying the values from the later files over the the
/// current parsed state:
/// * The file /etc/scratch.toml
/// * The file ~/.scratch.toml
/// * The file identified by the $SCRATCH_CONFIG_PATH environmental
///   variable.
/// Example configuration file might look like:
///
/// ```
/// template = "/data/users/$REPO_OWNER_USER/scratch"
/// overrides = {"/data/users/wez/fbsource": "/dev/shm/scratch"}
/// ```
#[derive(Debug, Deserialize, Default)]
struct Config {
    /// An optional "template" path.  Template paths are subject to
    /// two simple substitution transformations; $HOME is expanded
    /// to the home directory of the current user and $USER is
    /// expanded to the user name of the current user.  This allows
    /// definition of a simple placement policy without explicitly
    /// specifying the value for every user.
    /// If left unspecified, the default value is equivalent to
    /// `$HOME/.scratch`.
    template: Option<String>,

    /// The list of overridden settings
    #[serde(default)]
    overrides: HashMap<String, String>,

    /// Scratch style. See [`ScratchStyle`]
    style: Option<ScratchStyle>,
}

/// Returns the home directory of the user as a string.
/// Will panic if it cannot be resolved, or cannot be represented
/// as UTF-8.
fn home_dir() -> String {
    let home = dirs::home_dir().expect("resolved HOME dir");
    home.to_str()
        .expect(&format!(
            "HOME dir {:?} was not representable as UTF-8",
            home
        ))
        .into()
}

#[cfg(unix)]
fn lookup_home_dir_for_user(user: &str) -> Result<String> {
    let pw = PasswordEntry::by_name(user)?;
    Ok(pw.home_dir)
}

/// This is technically wrong for windows, but is at least
/// wrong in a backwards compatible way
#[cfg(windows)]
fn lookup_home_dir_for_user(_user: &str) -> Result<String> {
    Ok(home_dir())
}

impl Config {
    /// Attempt to load a Config instance from the specified path.
    /// If path does not exist, None is returned.
    fn load_file<P: AsRef<Path>>(path: P) -> Result<Option<Self>> {
        let path = path.as_ref();
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(err) => match err.kind() {
                std::io::ErrorKind::NotFound => return Ok(None),
                _ => bail!(err),
            },
        };
        let mut s = String::new();
        file.read_to_string(&mut s)?;

        toml::from_str(&s)
            .map(|c| Some(c))
            .map_err(|e| format_err!("error while loading TOML from {}: {:?}", path.display(), e))
    }

    /// Merge the values from other into self.
    fn merge(&mut self, mut other: Self) {
        if let Some(template) = other.template.take() {
            self.template = Some(template);
        }

        self.overrides.extend(other.overrides.into_iter());

        if let Some(style) = other.style.take() {
            self.style = Some(style);
        }
    }

    /// Compute the effective configuration by loading the configuration
    /// files in order and merging them together.  Missing files are OK,
    /// but any IO or parse errors cause the config resolution to stop and
    /// return the error.
    fn load() -> Result<Self> {
        let mut result = Self::default();

        let config_files = [
            #[cfg(unix)]
            Some("/etc/scratch.toml".into()),
            #[cfg(windows)]
            Some("C:/ProgramData/facebook/scratch.toml".into()),
            Some(format!("{}/.scratch.toml", home_dir())),
            std::env::var("SCRATCH_CONFIG_PATH").ok(),
        ];
        for path in config_files.iter().filter_map(Option::as_ref) {
            if let Some(o) = Self::load_file(path)? {
                result.merge(o);
            }
        }

        Ok(result)
    }

    /// Look up the template string for a given repo path.
    /// This is taken from a matching `overrides` entry first, if any,
    /// then the global `template` configuration, if any, finally
    /// falling back to a default value of `$HOME/.scratch`.
    /// We use `$HOME` rather than `/tmp` as it less prone to
    /// bad actors mounting a symlink attack.
    fn template_for_path(&self, path: &Path, owner: &str) -> String {
        // First, let's see if we have an override for this path
        let path_str = path.to_str().expect("path must be UTF-8");
        if let Some(over) = self.overrides.get(path_str) {
            return over.clone();
        }
        match &self.template {
            &Some(ref s) => s.clone(),
            &None => {
                // This is a little bit of a hack; ideally we'd
                // configure this in chef, but don't have bandwidth
                // to prepare a recipe for this in time; will follow
                // up in T31633485.
                // If there is a /data/users/<owner> dir, then we
                // use that to hold the scratch dir.
                let local = format!("/data/users/{}", owner);
                if let Ok(meta) = fs::metadata(&local) {
                    if meta.is_dir() {
                        return format!("{}/scratch", local);
                    }
                }
                // Otherwise use their home dir
                format!("{}/.scratch", home_dir())
            }
        }
    }
}

fn run() -> Result<()> {
    let matches = App::new("Scratch")
        .setting(AppSettings::SubcommandRequired)
        .setting(AppSettings::ColoredHelp)
        .version("1.0")
        .author("Source Control <oncall+source_control@xmail.facebook.com")
        .arg(
            Arg::with_name("no-create")
                .long("no-create")
                .short("n")
                .help("Do not create files or directories"),
        )
        .subcommand(
            SubCommand::with_name("path")
                .about("create and display the scratch path corresponding to the input path")
                .arg(
                    Arg::with_name("subdir")
                        .long("subdir")
                        .help("generate an isolated subdir based off this string")
                        .value_name("PATH"),
                )
                .arg(
                    Arg::with_name("watchable")
                        .long("watchable")
                        .help("the returned scratch space needs to be watchable by watchman"),
                )
                .arg(
                    Arg::with_name("REPO")
                        .help(
                            "Specifies the path to the repo. \
                             If omitted, infer the path from the current working directory",
                        )
                        .index(1),
                ),
        )
        .get_matches();

    let no_create = matches.is_present("no-create");

    let config = Config::load()?;

    match matches.subcommand() {
        ("path", Some(cmd)) => {
            let subdir = cmd.value_of("subdir");
            let watchable = cmd.is_present("watchable");
            let repo = cmd.value_of("REPO");
            path_command(&config, no_create, subdir, watchable, repo)
        }
        // AppSettings::SubcommandRequired should mean that this is unpossible
        _ => unreachable!("wut?"),
    }
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(err) => {
            eprintln!("scratch failed: {}", err);
            std::process::exit(1)
        }
    }
}

/// Returns the current username, falling back to the literal
/// string `$USER` for env var expansion.
fn get_current_user() -> String {
    let env_name = if cfg!(windows) { "USERNAME" } else { "USER" };
    env::var(env_name).unwrap_or_else(|_| "$USER".into())
}

/// Given an absolute path, locate the repository root.
fn locate_repo_root(path: &Path) -> Option<&Path> {
    path.ancestors()
        .filter(|p| p.join(".hg").is_dir() || p.join(".git").exists())
        .nth(0)
}

#[cfg(unix)]
struct PasswordEntry {
    unixname: String,
    home_dir: String,
    uid: u32,
    gid: u32,
}

#[cfg(unix)]
impl PasswordEntry {
    fn maybe_string(cstr: *const libc::c_char, context: &str) -> Result<String> {
        if cstr.is_null() {
            bail!(context.to_string());
        } else {
            let cstr = unsafe { std::ffi::CStr::from_ptr(cstr) };
            cstr.to_str().map_err(|e| e.into()).map(|x| x.to_owned())
        }
    }

    fn from_password(pwent: *const libc::passwd) -> Result<Self> {
        ensure!(!pwent.is_null(), "password ptr is null");
        let pw = unsafe { &*pwent };
        Ok(Self {
            unixname: Self::maybe_string(pw.pw_name, "pw_name is null")?,
            home_dir: Self::maybe_string(pw.pw_dir, "pw_dir is null")?,
            uid: pw.pw_uid,
            gid: pw.pw_gid,
        })
    }

    /// Lookup a PasswordEntry for a uid.
    /// Not thread safe.
    pub fn by_uid(uid: u32) -> Result<Self> {
        let pw = unsafe { libc::getpwuid(uid) };
        if pw.is_null() {
            let err = std::io::Error::last_os_error();
            bail!("getpwuid({}) failed: {}", uid, err);
        }
        Self::from_password(pw)
    }

    /// Lookup a PasswordEntry for a unix username.
    /// Not thread safe.
    pub fn by_name(unixname: &str) -> Result<Self> {
        let user_cstr = std::ffi::CString::new(unixname.to_string())?;

        let pw = unsafe { libc::getpwnam(user_cstr.as_ptr()) };
        if pw.is_null() {
            let err = std::io::Error::last_os_error();
            bail!("getpwnam({}) failed: {}", unixname, err);
        }

        Self::from_password(pw)
    }
}

/// Given a path, return the unix name of the owner of that path.
/// If we cannot stat the path, raise an error.
#[cfg(unix)]
fn get_file_owner(path: &Path) -> Result<String> {
    let meta = fs::metadata(path)
        .map_err(|e| format_err!("unable to get metadata for {}: {}", path.display(), e))?;
    let uid = meta.uid();
    let pw = PasswordEntry::by_uid(uid)?;

    Ok(pw.unixname)
}

#[cfg(unix)]
fn set_file_owner(path: &Path, owner: &str) -> Result<()> {
    use std::ffi::CString;

    let is_root = unsafe { libc::geteuid() } == 0;

    if !is_root {
        // Can't change the ownership, so stick with who we are
        return Ok(());
    }

    let pw = PasswordEntry::by_name(owner)?;

    let path_cstr =
        CString::new(path.to_str().ok_or_else(|| {
            format_err!("path {} cannot be represented as String", path.display())
        })?)?;
    let result = unsafe { libc::chown(path_cstr.as_ptr(), pw.uid, pw.gid) };
    if result != 0 {
        let err = std::io::Error::last_os_error();
        bail!(
            "Failed to chown({}, {} (uid={}, gid={})): {}",
            path.display(),
            owner,
            pw.uid,
            pw.gid,
            err
        );
    }

    Ok(())
}

/// This should alter the file ACLs on windows, but for now we're just
/// ignoring this, as we don't think the issue a practical problem.
#[cfg(windows)]
fn set_file_owner(_path: &Path, _owner: &str) -> Result<()> {
    Ok(())
}

/// This should return the owner of a path, but for now it just returns
/// the current user name on Windows systems.  This is probably correct
/// and good enough for the moment, and we can add support for the real
/// thing in a later diff.
#[cfg(windows)]
fn get_file_owner(_path: &Path) -> Result<String> {
    Ok(get_current_user())
}

/// Resolves the root directory to use as the scratch space for a given
/// repository path.  This is the function that performs expansion of
/// the $USER and $HOME placeholder tokens in the configured template.
fn scratch_root(config: &Config, path: &Path) -> Result<PathBuf> {
    let repo_owner = get_file_owner(path)?;
    let template = config.template_for_path(path, &repo_owner);

    let user = get_current_user();
    let home = home_dir();
    let repo_owner_home = lookup_home_dir_for_user(&repo_owner)?;

    let mut root = PathBuf::from(
        template
            .replace("$REPO_OWNER_USER", &repo_owner)
            .replace("$REPO_OWNER_HOME", &repo_owner_home)
            .replace("$USER", &user)
            .replace("$HOME", &home),
    );

    root.push(encode(
        path.to_str()
            .ok_or(format_err!("{:?} cannot be converted to utf8", path))?,
    ));
    Ok(root)
}

/// A watchable path needs a .watchmanconfig file to define the boundary
/// of the watch and allow the watch of occur.
fn create_watchmanconfig(_config: &Config, path: &Path, repo_owner: &str) -> Result<()> {
    let filename = path.join(".watchmanconfig");
    let mut file = fs::File::create(&filename)?;
    // Write out an empty json object
    file.write_all("{}".as_bytes())?;
    set_file_owner(&filename, repo_owner)?;
    Ok(())
}

/// Validates curdir parameter. When using Mirror style scratch space, it is possible to escape the
/// scratch directory by passing in path with `..` component.
fn valid_curdir(p: &Path) -> bool {
    p.components().all(|c| c != Component::ParentDir)
}

#[test]
fn test_valid_curdir() {
    assert!(valid_curdir("a/b/c".as_ref()));
    assert!(valid_curdir("/a/b/c".as_ref()));
    assert!(valid_curdir("c:\\abc\\def".as_ref()));
    assert!(valid_curdir("./abc/./abc".as_ref()));
    assert!(!valid_curdir("../abc/../abc".as_ref()));
    assert!(!valid_curdir("abc/../abc".as_ref()));
}

/// Checks if the scratch root path has a README.txt file.
/// If the scratch root path does not exist then successfully returns.
/// If the file exists then successfully returns.
/// If README.txt exists but it is not a file then returns an error.
/// If README.txt does not exist then attemts to create it and reports the status.
fn readme_in_scratch_path(scratch_root_path: &Path) -> Result<()> {
    if !scratch_root_path.exists() {
        return Ok(());
    }

    let readme_path = scratch_root_path.join("README.txt");

    if readme_path.exists() {
        return match readme_path.is_file() {
            true => Ok(()),
            false => Err(anyhow::anyhow!("README.txt exists but it is not a file.")),
        };
    }

    fs::File::create(readme_path)?.write_all(
        b"This directory is created to store build artifacts. \
        It is commonly used by Buck and other build systems. \
        It is okay to delete files from this directory \
        but it is recommended to clean with buck clean and similar commands.",
    )?;
    Ok(())
}

/// Performs the `path` command
fn path_command(
    config: &Config,
    no_create: bool,
    subdir: Option<&str>,
    watchable: bool,
    path: Option<&str>,
) -> Result<()> {
    // Canonicalize the provided path.  If no path was provided, fall
    // back to the cwd.
    let path = match path {
        Some(path) => fs::canonicalize(path)
            .map_err(|e| format_err!("unable to canonicalize path: {}: {}", path, e))?,
        None => env::current_dir()?,
    };

    // Resolve the path to the corresponding repo root.
    // If the path is not a repo then we use the provided path.
    let repo_root = locate_repo_root(&path).unwrap_or(&path);

    // Get the base scratch path for this repo
    let mut result = scratch_root(&config, repo_root)?;
    readme_in_scratch_path(&result)?;
    let repo_owner = get_file_owner(repo_root)?;

    // If they asked for a subdir, compute it
    if let Some(subdir) = subdir {
        if watchable {
            result.push("watchable");
        }

        if let Some(ScratchStyle::Mirror) = config.style {
            if valid_curdir(subdir.as_ref()) {
                result.push(subdir);
            } else {
                bail!("subdir path contains parent component: {:?}", subdir);
            }
        } else {
            result.push(encode(subdir));
        }
    }

    if !no_create {
        let mut ancestors = result.ancestors().collect::<Vec<_>>();
        ancestors.reverse();
        for ancestor in ancestors.iter() {
            match fs::create_dir(ancestor) {
                Ok(()) => set_file_owner(&ancestor, &repo_owner)?,
                Err(_) if ancestor.is_dir() => {}
                Err(e) => bail!(e),
            }
        }
        if watchable {
            create_watchmanconfig(&config, &result, &repo_owner)?;
        }
    }

    println!("{}", result.display());
    Ok(())
}

/// Normalizes various path format on Windows. This function will convert
/// various Windows path format to full path form. Note this function does not
/// canonicalize the given path. So it does not collapse dots nor expand
/// relative paths.
/// Ref: https://googleprojectzero.blogspot.com/2016/02/the-definitive-guide-on-win32-to-nt.html
#[cfg(windows)]
fn normalize_windows_path(path: &str) -> String {
    let path = path.replace("/", r"\");
    if let Some(path) = path.strip_prefix(r"\??\UNC\") {
        // NT UNC path
        format!(r"\\{}", path)
    } else if let Some(path) = path.strip_prefix(r"\??\") {
        // NT path
        path.to_owned()
    } else if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        // Extend-length UNC path
        format!(r"\\{}", path)
    } else if let Some(path) = path.strip_prefix(r"\\?\") {
        // Extend-length path
        path.to_owned()
    } else {
        path
    }
}

/// Given a string representation of a path, encode it such that all
/// file/path special characters are replaced with non-special characters.
/// This has the effect of flattening a relative path fragment like
/// `foo/bar` into a single level path component like `fooZbar`.
/// Scratch uses this to give the appearance of hierarchy to clients
/// without having an actual hierarchy.  This is important on systems
/// such as Windows and macOS where the filesystem watchers are always
/// recursive.
/// The mapping is not and does not need to be reversible.
/// Why not just compute a SHA or MD5 hash?  It is nicer for the user
/// to have an idea of what the path is when they list the scratch container
/// path, which is something they'll likely end up doing when their disk
/// gets full, and they'll appreciate knowing which of these dirs have
/// value to them.
fn encode(path: &str) -> String {
    let mut result = String::with_capacity(path.len());

    // `std::fs::canonicalize` on Windows will normalize path into
    // extended-length format, which has a prefix `\\?\`. This function will
    // incorrect generate a path with the question mark which is an invalid
    // path.
    #[cfg(windows)]
    let path = &normalize_windows_path(path);

    for (i, b) in path.chars().enumerate() {
        if cfg!(unix) && i == 0 && b == '/' {
            // On unix, most paths begin with a slash, which
            // means that we'd use a Z prefix everything.
            // Let's just skip the first character.
            continue;
        }
        match b {
            '/' | '\\' => result.push('Z'),
            'Z' => result.push_str("_Z"),
            ':' => result.push_str("_"),
            _ => result.push(b),
        }
    }

    result
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_encode() {
        if cfg!(unix) {
            assert_eq!(encode("/foo/bar"), "fooZbar");
        } else {
            assert_eq!(encode("/foo/bar"), "ZfooZbar");
        }
        assert_eq!(encode("foo"), "foo");
        assert_eq!(encode("foo/bar"), "fooZbar");
        assert_eq!(encode(r"foo\bar"), "fooZbar");
        assert_eq!(encode("fooZbar"), "foo_Zbar");
        assert_eq!(encode("foo_Zbar"), "foo__Zbar");
        assert_eq!(encode(r"C:\foo\bar"), "C_ZfooZbar");
        assert_eq!(encode(r"\\unc\path"), "ZZuncZpath");

        if cfg!(windows) {
            assert_eq!(encode(r"\\?\C:\foo\bar"), "C_ZfooZbar");
            assert_eq!(encode(r"\\?\UNC\unc\path"), "ZZuncZpath");
            assert_eq!(encode(r"\??\C:\foo\bar"), "C_ZfooZbar");
            assert_eq!(encode(r"\??\UNC\unc\path"), "ZZuncZpath");
        }
    }

    #[cfg(windows)]
    #[test]
    fn test_normalize_windows_path() {
        assert_eq!(normalize_windows_path(r"c:\foo\bar"), r"c:\foo\bar");
        assert_eq!(normalize_windows_path(r"c:/foo/bar"), r"c:\foo\bar");
        assert_eq!(normalize_windows_path(r"\??\c:\foo\bar"), r"c:\foo\bar");
        assert_eq!(normalize_windows_path(r"\\?\c:\foo\bar"), r"c:\foo\bar");
        assert_eq!(
            normalize_windows_path(r"\??\UNC\server\foo\bar"),
            r"\\server\foo\bar"
        );
        assert_eq!(
            normalize_windows_path(r"\\?\UNC\server\foo\bar"),
            r"\\server\foo\bar"
        );
    }
}
