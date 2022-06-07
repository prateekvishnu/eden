/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod print;

use anyhow::Result;
use clidispatch::errors;
use clidispatch::io::CanColor;
use clidispatch::io::IO;
use cliparser::define_flags;
use print::PrintConfig;
use print::PrintConfigStatusTypes;
use repo::repo::Repo;
use types::path::RepoPathRelativizer;

use crate::commands::FormatterOpts;
use crate::commands::WalkOpts;

define_flags! {
    pub struct StatusOpts {
        /// show status of all files
        #[short('A')]
        all: bool,

        /// show only modified files
        #[short('m')]
        modified: bool,

        /// show only added files
        #[short('a')]
        added: bool,

        /// show only removed files
        #[short('r')]
        removed: bool,

        /// show only deleted (but tracked) files
        #[short('d')]
        deleted: bool,

        /// show only files without changes
        #[short('c')]
        clean: bool,

        /// show only unknown (not tracked) files
        #[short('u')]
        unknown: bool,

        /// show only ignored files
        #[short('i')]
        ignored: bool,

        /// hide status prefix
        #[short('n')]
        no_status: bool,

        /// show the terse output (EXPERIMENTAL)
        #[short('t')]
        terse: String,

        /// show source of copied files
        #[short('C')]
        copies: bool,

        /// end filenames with NUL, for use with xargs
        #[short('0')]
        print0: bool,

        /// show difference from revision
        rev: Vec<String>,

        /// list the changed files of a revision
        change: String,

        /// show status relative to root
        root_relative: bool,

        walk_opts: WalkOpts,
        formatter_opts: FormatterOpts,

        #[args]
        args: Vec<String>,
    }
}

pub fn run(opts: StatusOpts, io: &IO, repo: &mut Repo) -> Result<u8> {
    let rev_check = opts.rev.is_empty() || (opts.rev.len() == 1 && opts.rev[0] == ".");

    let args_check = opts.args.is_empty() || (opts.args.len() == 1 && opts.args[0] == "re:.");

    if opts.all
        || !opts.change.is_empty()
        || !opts.terse.is_empty()
        || !rev_check
        || !opts.walk_opts.include.is_empty()
        || !opts.walk_opts.exclude.is_empty()
        || !opts.formatter_opts.template.is_empty()
        || !args_check
    {
        return Err(errors::FallbackToPython(name()).into());
    }

    let StatusOpts {
        modified,
        added,
        removed,
        deleted,
        clean,
        unknown,
        ignored,
        ..
    } = opts;

    let status_types = if modified || added || removed || deleted || clean || unknown || ignored {
        PrintConfigStatusTypes {
            modified,
            added,
            removed,
            deleted,
            clean,
            unknown,
            ignored,
        }
    } else {
        PrintConfigStatusTypes {
            modified: true,
            added: true,
            removed: true,
            deleted: true,
            clean: false,
            unknown: true,
            ignored: false,
        }
    };
    let print_config = PrintConfig {
        status_types,
        no_status: opts.no_status,
        copies: opts.copies,
        endl: if opts.print0 { '\0' } else { '\n' },
        root_relative: opts.root_relative,
        use_color: io.output().can_color(),
    };

    // Attempt to fetch status information from EdenFS.
    let (status, copymap) = edenfs_client::status::maybe_status_fastpath(
        repo.path(),
        io,
        print_config.status_types.ignored,
    )?;

    let cwd = std::env::current_dir()?;
    let relativizer = RepoPathRelativizer::new(cwd, repo.path());
    print::print_status(io, relativizer, &print_config, &status, &copymap)?;
    Ok(0)
}

pub fn name() -> &'static str {
    "status|st|sta|stat|statu"
}

pub fn doc() -> &'static str {
    r#"list files with pending changes

    Show status of files in the repository using the following status
    indicators::

      M = modified
      A = added
      R = removed
      C = clean
      ! = missing (deleted by a non-hg command, but still tracked)
      ? = not tracked
      I = ignored
        = origin of the previous file (with --copies)

    By default, shows files that have been modified, added, removed,
    deleted, or that are unknown (corresponding to the options -mardu).
    Files that are unmodified, ignored, or the source of a copy/move
    operation are not listed.

    To control the exact statuses that are shown, specify the relevant
    flags (like -rd to show only files that are removed or deleted).
    Additionally, specify -q/--quiet to hide both unknown and ignored
    files.

    To show the status of specific files, provide an explicit list of
    files to match. To include or exclude files using regular expressions,
    use -I or -X.

    If --rev is specified, and only one revision is given, it is used as
    the base revision. If two revisions are given, the differences between
    them are shown. The --change option can also be used as a shortcut
    to list the changed files of a revision from its first parent.

    .. note::

       :hg:`status` might appear to disagree with :hg:`diff` if permissions
       have changed or a merge has occurred, because the standard diff
       format does not report permission changes and :hg:`diff` only
       reports changes relative to one merge parent.

    .. container:: verbose

      The -t/--terse option abbreviates the output by showing only the directory
      name if all the files in it share the same status. The option takes an
      argument indicating the statuses to abbreviate: 'm' for 'modified', 'a'
      for 'added', 'r' for 'removed', 'd' for 'deleted', 'u' for 'unknown', 'i'
      for 'ignored' and 'c' for clean.

      It abbreviates only those statuses which are passed. Note that clean and
      ignored files are not displayed with '--terse ic' unless the -c/--clean
      and -i/--ignored options are also used.

      The -v/--verbose option shows information when the repository is in an
      unfinished merge, shelve, rebase state etc. You can have this behavior
      turned on by default by enabling the ``commands.status.verbose`` option.

      You can skip displaying some of these states by setting
      ``commands.status.skipstates`` to one or more of: 'bisect', 'graft',
      'histedit', 'merge', 'rebase', or 'unshelve'.

      Examples:

      - show changes in the working directory relative to a
        changeset::

          hg status --rev 9353

      - show changes in the working directory relative to the
        current directory (see :hg:`help patterns` for more information)::

          hg status re:

      - show all changes including copies in an existing changeset::

          hg status --copies --change 9353

      - get a NUL separated list of added files, suitable for xargs::

          hg status -an0

      - show more information about the repository status, abbreviating
        added, removed, modified, deleted, and untracked paths::

          hg status -v -t mardu

    Returns 0 on success."#
}

pub fn synopsis() -> Option<&'static str> {
    Some("[OPTION]... [FILE]...")
}
