/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io;
use std::io::BufRead;
use std::io::BufReader;

use futures::future::FutureExt;
use futures::future::LocalBoxFuture;
use futures::Future;
use types::RepoPath;

#[derive(Default, Debug)]
pub struct Profile {
    // Where this profile came from (typically a file path).
    source: String,

    // [include], [exclude] and %include
    entries: Vec<ProfileEntry>,

    // [metadata]
    title: Option<String>,
    description: Option<String>,
    hidden: Option<String>,
    version: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Pattern {
    Include(String),
    Exclude(String),
}

#[derive(Debug)]
enum ProfileEntry {
    // Pattern plus additional source for this rule (e.g. "hgrc.dynamic").
    Pattern(Pattern, Option<String>),
    Profile(String),
}

#[derive(PartialEq)]
enum SectionType {
    Include,
    Exclude,
    Metadata,
}

impl Pattern {
    fn as_str(&self) -> &str {
        match self {
            Self::Include(p) => p,
            Self::Exclude(p) => p,
        }
    }
}

impl SectionType {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "[include]" => Some(SectionType::Include),
            "[exclude]" => Some(SectionType::Exclude),
            "[metadata]" => Some(SectionType::Metadata),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("import cycle involving {0}")]
    ImportCycle(String),

    #[error(transparent)]
    Fetch(#[from] anyhow::Error),

    #[error("unsuppported pattern type {0}")]
    UnsupportedPattern(String),

    #[error(transparent)]
    GlobsetError(#[from] globset::Error),
}

impl Profile {
    pub fn from_bytes(data: impl AsRef<[u8]>, source: String) -> Result<Self, io::Error> {
        let mut prof: Profile = Default::default();
        let mut current_metadata_val: Option<&mut String> = None;
        let mut section_type = SectionType::Include;
        let mut dynamic_source: Option<String> = None;

        for (mut line_num, line) in BufReader::new(data.as_ref()).lines().enumerate() {
            line_num += 1;

            let line = line?;
            let trimmed = line.trim();

            // Ingore comments and emtpy lines.
            let mut chars = trimmed.chars();
            match chars.next() {
                None => continue,
                Some('#' | ';') => {
                    let comment = chars.as_str().trim();
                    if let Some((l, r)) = comment.split_once(&['=', ':']) {
                        match (l.trim(), r.trim()) {
                            // Allow a magic comment to specify additional
                            // source information for particular rules. This way
                            // it is backwards compatible with the python code
                            // if a config like this ever gets written out.
                            ("source", "") => dynamic_source = None,
                            ("source", src) => dynamic_source = Some(src.to_string()),
                            _ => {}
                        }
                    }
                    continue;
                }
                _ => {}
            }

            if let Some(p) = trimmed.strip_prefix("%include ") {
                prof.entries
                    .push(ProfileEntry::Profile(p.trim().to_string()));
            } else if let Some(section_start) = SectionType::from_str(trimmed) {
                section_type = section_start;
                current_metadata_val = None;
            } else if section_type == SectionType::Metadata {
                if line.starts_with(&[' ', '\t']) {
                    // Continuation of multiline value.
                    if let Some(ref mut val) = current_metadata_val {
                        val.push('\n');
                        val.push_str(trimmed);
                    } else {
                        tracing::warn!(%line, %source, line_num, "orphan metadata line");
                    }
                } else {
                    current_metadata_val = None;
                    if let Some((key, val)) = trimmed.split_once(&['=', ':']) {
                        let prof_val = match key.trim() {
                            "description" => &mut prof.description,
                            "title" => &mut prof.title,
                            "hidden" => &mut prof.hidden,
                            "version" => &mut prof.version,
                            _ => {
                                tracing::warn!(%line, %source, line_num, "ignoring uninteresting metadata key");
                                continue;
                            }
                        };

                        current_metadata_val = Some(prof_val.insert(val.trim().to_string()));
                    }
                }
            } else {
                if trimmed.starts_with('/') {
                    tracing::warn!(%line, %source, line_num, "ignoring sparse rule starting with /");
                    continue;
                }

                if section_type == SectionType::Include {
                    prof.entries.push(ProfileEntry::Pattern(
                        Pattern::Include(trimmed.to_string()),
                        dynamic_source.clone(),
                    ));
                } else {
                    prof.entries.push(ProfileEntry::Pattern(
                        Pattern::Exclude(trimmed.to_string()),
                        dynamic_source.clone(),
                    ));
                }
            }
        }

        prof.source = source;

        Ok(prof)
    }

    fn is_v2(&self) -> bool {
        if let Some(version) = &self.version {
            version == "2"
        } else {
            false
        }
    }

    // Recursively flatten this profile into a DFS ordered list of rules.
    // %import statements are resolved by fetching the imported profile's
    // contents using the fetch callback. Returns a vec of each Pattern paired
    // with a String describing its provenance.
    async fn rules<B: Future<Output = anyhow::Result<Option<Vec<u8>>>>>(
        &self,
        mut fetch: impl FnMut(String) -> B,
    ) -> Result<Vec<(Pattern, String)>, Error> {
        fn rules_inner<'a, B: Future<Output = anyhow::Result<Option<Vec<u8>>>>>(
            prof: &'a Profile,
            fetch: &'a mut dyn FnMut(String) -> B,
            rules: &'a mut Vec<(Pattern, String)>,
            source: Option<&'a str>,
            // path => (contents, in_progress)
            seen: &'a mut HashMap<String, (Vec<u8>, bool)>,
        ) -> LocalBoxFuture<'a, Result<(), Error>> {
            async move {
                let source = match source {
                    Some(history) => format!("{} -> {}", history, prof.source),
                    None => prof.source.clone(),
                };

                for entry in prof.entries.iter() {
                    match entry {
                        ProfileEntry::Pattern(p, psrc) => {
                            rules.push((p.clone(), join_source(source.clone(), psrc.as_deref())))
                        }
                        ProfileEntry::Profile(child_path) => {
                            let entry = seen.entry(child_path.clone());
                            let data = match entry {
                                Entry::Occupied(e) => match e.into_mut() {
                                    (_, true) => {
                                        return Err(Error::ImportCycle(child_path.clone()));
                                    }
                                    (data, false) => data,
                                },
                                Entry::Vacant(e) => {
                                    if let Some(data) = fetch(child_path.clone()).await? {
                                        &e.insert((data, true)).0
                                    } else {
                                        continue;
                                    }
                                }
                            };

                            let mut child = Profile::from_bytes(&data, child_path.clone())?;
                            rules_inner(&mut child, fetch, rules, Some(&source), seen).await?;

                            if let Some((_, in_progress)) = seen.get_mut(child_path) {
                                *in_progress = false;
                            }
                        }
                    }
                }

                Ok(())
            }
            .boxed_local()
        }

        let mut rules = Vec::new();
        rules_inner(self, &mut fetch, &mut rules, None, &mut HashMap::new()).await?;
        Ok(rules)
    }

    pub async fn matcher<B: Future<Output = anyhow::Result<Option<Vec<u8>>>>>(
        &self,
        mut fetch: impl FnMut(String) -> B,
    ) -> Result<Matcher, Error> {
        if self.entries.is_empty() {
            return Ok(Matcher::always());
        }

        let mut matchers: Vec<pathmatcher::TreeMatcher> = Vec::new();

        // List of rule origins per-matcher.
        let mut rule_origins: Vec<Vec<String>> = Vec::new();

        let mut rules: VecDeque<(Pattern, String)> = VecDeque::new();

        // Maintain the excludes-come-last ordering.
        let mut push_rule = |(pat, src)| match pat {
            Pattern::Exclude(_) => rules.push_back((pat, src)),
            Pattern::Include(_) => rules.push_front((pat, src)),
        };

        let prepare_rules =
            |rules: VecDeque<(Pattern, String)>| -> Result<(Vec<String>, Vec<String>), Error> {
                let mut matcher_rules = Vec::new();
                let mut origins = Vec::new();

                for (pat, src) in rules {
                    for expanded_rule in sparse_pat_to_matcher_rule(pat)? {
                        matcher_rules.push(expanded_rule);
                        origins.push(src.clone());
                    }
                }

                Ok((matcher_rules, origins))
            };

        let mut only_v1 = true;
        for entry in self.entries.iter() {
            match entry {
                ProfileEntry::Pattern(p, src) => {
                    push_rule((p.clone(), join_source(self.source.clone(), src.as_deref())))
                }
                ProfileEntry::Profile(child_path) => {
                    let child = match fetch(child_path.clone()).await? {
                        Some(data) => Profile::from_bytes(data, child_path.clone())?,
                        None => continue,
                    };

                    let child_rules: VecDeque<(Pattern, String)> = child
                        .rules(&mut fetch)
                        .await?
                        .into_iter()
                        .map(|(p, s)| (p, format!("{} -> {}", self.source, s)))
                        .collect();

                    // TODO(muirdm): make this only happen for root profile.
                    if child.is_v2() {
                        only_v1 = false;

                        let (matcher_rules, origins) = prepare_rules(child_rules)?;
                        matchers.push(pathmatcher::TreeMatcher::from_rules(matcher_rules.iter())?);
                        rule_origins.push(origins);
                    } else {
                        for rule in child_rules {
                            push_rule(rule);
                        }
                    }
                }
            }
        }

        // If all user specified rules are exclude rules, add an
        // implicit "**" to provide the default include of everything.
        if only_v1 && (rules.is_empty() || matches!(&rules[0].0, Pattern::Exclude(_))) {
            rules.push_front((Pattern::Include("**".to_string()), "(builtin)".to_string()))
        }

        rules.push_front((
            Pattern::Include("glob:.hg*".to_string()),
            "(builtin)".to_string(),
        ));

        let (matcher_rules, origins) = prepare_rules(rules)?;
        matchers.push(pathmatcher::TreeMatcher::from_rules(matcher_rules.iter())?);
        rule_origins.push(origins);

        Ok(Matcher::new(matchers, rule_origins))
    }
}

fn join_source(main_source: String, opt_source: Option<&str>) -> String {
    match opt_source {
        None => main_source,
        Some(opt) => format!("{} ({})", main_source, opt),
    }
}

pub struct Matcher {
    always: bool,
    matchers: Vec<pathmatcher::TreeMatcher>,
    // List of rule origins per-matcher.
    rule_origins: Vec<Vec<String>>,
}

impl Matcher {
    pub fn matches(&self, path: &RepoPath) -> anyhow::Result<bool> {
        if self.always {
            Ok(true)
        } else {
            let result = pathmatcher::UnionMatcher::matches_file(self.matchers.iter(), path);
            tracing::trace!(%path, ?result, "matches");
            result
        }
    }

    pub fn explain(&self, path: &RepoPath) -> anyhow::Result<(bool, String)> {
        if self.always {
            return Ok((true, "implicit match due to empty profile".to_string()));
        }

        for (i, m) in self.matchers.iter().enumerate() {
            if let Some(idx) = m.matching_rule_indexes(path.as_str()).last() {
                let rule_origin = self
                    .rule_origins
                    .get(i)
                    .and_then(|o| o.get(*idx))
                    .map_or("(unknown)".to_string(), |o| o.clone());
                return Ok((m.matches(path.as_str()), rule_origin));
            }
        }

        Ok((false, "no rules matched".to_string()))
    }
}

impl pathmatcher::Matcher for Matcher {
    fn matches_directory(&self, path: &RepoPath) -> anyhow::Result<pathmatcher::DirectoryMatch> {
        if self.always {
            Ok(pathmatcher::DirectoryMatch::Everything)
        } else {
            let result = pathmatcher::UnionMatcher::matches_directory(self.matchers.iter(), path);
            tracing::trace!(%path, ?result, "matches_directory");
            result
        }
    }

    fn matches_file(&self, path: &RepoPath) -> anyhow::Result<bool> {
        self.matches(path)
    }
}

impl Matcher {
    fn new(matchers: Vec<pathmatcher::TreeMatcher>, rule_origins: Vec<Vec<String>>) -> Self {
        Self {
            always: false,
            matchers,
            rule_origins,
        }
    }
    fn always() -> Self {
        Self {
            always: true,
            rule_origins: Vec::new(),
            matchers: Vec::new(),
        }
    }
}

static ALL_PATTERN_KINDS: &[&str] = &[
    "re",
    "glob",
    "path",
    "relglob",
    "relpath",
    "relre",
    "listfile",
    "listfile0",
    "set",
    "include",
    "subinclude",
    "rootfilesin",
];

// Convert a sparse profile pattern into what the tree matcher
// expects. We only support "glob" and "path" pattern types.
fn sparse_pat_to_matcher_rule(pat: Pattern) -> Result<Vec<String>, Error> {
    static DEFAULT_TYPE: &str = "glob";

    let (pat_type, pat_text) = match pat.as_str().split_once(':') {
        Some((t, p)) => match t {
            "glob" | "path" => (t, p),
            _ => {
                if ALL_PATTERN_KINDS.contains(&t) {
                    return Err(Error::UnsupportedPattern(t.to_string()));
                } else {
                    (DEFAULT_TYPE, pat.as_str())
                }
            }
        },
        None => (DEFAULT_TYPE, pat.as_str()),
    };

    let pats = match pat_type {
        "glob" => pathmatcher::expand_curly_brackets(pat_text)
            .iter()
            .map(|s| pathmatcher::normalize_glob(s))
            .collect(),
        "path" => vec![pathmatcher::plain_to_glob(pat_text)],
        _ => unreachable!(),
    };

    let make_recursive = |p: String| -> String {
        if p.is_empty() || p.ends_with('/') {
            p + "**"
        } else {
            p + "/**"
        }
    };

    Ok(pats
        .into_iter()
        // Adjust glob to ensure sparse rules match everything below them.
        .map(make_recursive)
        .map(|p| match pat {
            Pattern::Exclude(_) => format!("!{}", p),
            Pattern::Include(_) => p,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;

    // Returns a profile's (includes, excludes, profiles).
    fn split_prof(prof: &Profile) -> (Vec<&str>, Vec<&str>, Vec<&str>) {
        let (mut inc, mut exc, mut profs) = (vec![], vec![], vec![]);
        for entry in &prof.entries {
            match entry {
                ProfileEntry::Pattern(Pattern::Include(p), _) => inc.push(p.as_ref()),
                ProfileEntry::Pattern(Pattern::Exclude(p), _) => exc.push(p.as_ref()),
                ProfileEntry::Profile(p) => profs.push(p.as_ref()),
            }
        }
        (inc, exc, profs)
    }

    #[test]
    fn test_parsing() {
        let got = Profile::from_bytes(
            b"
; hello
  # there

a
[metadata]
boring = banana
title  =   foo
[include]
glob:b/**/z
/skip/me
%include  other.sparse
 [exclude]
c
/skip/me

[metadata]
	skip me
description:howdy
 doody
version : 123
hidden=your eyes
	only

",
            "test".to_string(),
        )
        .unwrap();

        assert_eq!(got.source, "test");

        let (inc, exc, profs) = split_prof(&got);
        assert_eq!(inc, vec!["a", "glob:b/**/z"]);
        assert_eq!(exc, vec!["c"]);
        assert_eq!(profs, vec!["other.sparse"]);

        assert_eq!(got.title.unwrap(), "foo");
        assert_eq!(got.description.unwrap(), "howdy\ndoody");
        assert_eq!(got.hidden.unwrap(), "your eyes\nonly");
        assert_eq!(got.version.unwrap(), "123");
    }

    #[tokio::test]
    async fn test_rules() -> anyhow::Result<()> {
        let base = b"
%include child

[include]
a

[metadata]
title = base
";

        let child = b"
%include grand_child

[include]
b

[metadata]
title = child
";

        let grand_child = b"
[include]
c

[metadata]
title = grand_child
";

        let base_prof = Profile::from_bytes(base, "test".to_string()).unwrap();

        let rules = base_prof
            .rules(|path| async move {
                match path.as_ref() {
                    "child" => Ok(Some(child.to_vec())),
                    "grand_child" => Ok(Some(grand_child.to_vec())),
                    _ => Err(anyhow!("not found")),
                }
            })
            .await?;

        assert_eq!(
            rules,
            vec![
                (
                    Pattern::Include("c".to_string()),
                    "test -> child -> grand_child".to_string()
                ),
                (
                    Pattern::Include("b".to_string()),
                    "test -> child".to_string()
                ),
                (Pattern::Include("a".to_string()), "test".to_string())
            ]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_recursive_imports() {
        let a = b"%include b";
        let b = b"%include a";

        let a_prof = Profile::from_bytes(a, "test".to_string()).unwrap();

        let res = a_prof
            .rules(|path| async move {
                match path.as_ref() {
                    "a" => Ok(Some(a.to_vec())),
                    "b" => Ok(Some(b.to_vec())),
                    _ => Err(anyhow!("not found")),
                }
            })
            .await;

        assert_eq!(format!("{}", res.unwrap_err()), "import cycle involving b");
    }

    #[tokio::test]
    async fn test_resolve_imports_caching() {
        let a = b"
%include b
%include b
";

        let a_prof = Profile::from_bytes(a, "test".to_string()).unwrap();

        let mut fetch_count = 0;

        // Make sure we cache results from the callback.
        let res = a_prof
            .rules(|_path| {
                fetch_count += 1;
                assert_eq!(fetch_count, 1);
                async { Ok(Some(vec![])) }
            })
            .await;

        assert!(res.is_ok());
    }

    #[test]
    fn test_sparse_pat_to_matcher_rule() {
        assert_eq!(
            sparse_pat_to_matcher_rule(Pattern::Include("path:/foo/bar".to_string())).unwrap(),
            vec!["/foo/bar/**"]
        );

        assert_eq!(
            sparse_pat_to_matcher_rule(Pattern::Include("/foo/*/bar{1,{2,3}}/".to_string()))
                .unwrap(),
            vec!["/foo/*/bar1/**", "/foo/*/bar2/**", "/foo/*/bar3/**"],
        );

        assert_eq!(
            sparse_pat_to_matcher_rule(Pattern::Include("path:/foo/*/bar{1,{2,3}}/".to_string()))
                .unwrap(),
            vec!["/foo/\\*/bar\\{1,\\{2,3\\}\\}/**"],
        );

        assert_eq!(
            sparse_pat_to_matcher_rule(Pattern::Exclude("glob:**".to_string())).unwrap(),
            vec!["!**/**"],
        );

        assert!(sparse_pat_to_matcher_rule(Pattern::Include("re:.*".to_string())).is_err());
    }

    #[tokio::test]
    async fn test_matcher_implicit_include() -> anyhow::Result<()> {
        let config = b"
[exclude]
path:exc
";

        let prof = Profile::from_bytes(config, "test".to_string()).unwrap();

        let matcher = prof.matcher(|_| async { Ok(Some(vec![])) }).await?;

        // Show we got an implicit rule that includes everything.
        assert!(matcher.matches("a/b".try_into()?)?);

        // Sanity that exclude works.
        assert!(!matcher.matches("exc/foo".try_into()?)?);

        Ok(())
    }

    #[tokio::test]
    async fn test_matcher_v1() -> anyhow::Result<()> {
        let base = b"
%include child

[exclude]
path:a/exc

[include]
path:a
";

        let child = b"
[exclude]
path:b/exc

[include]
path:b
";

        let prof = Profile::from_bytes(base, "test".to_string())?;
        let matcher = prof.matcher(|_| async { Ok(Some(child.to_vec())) }).await?;

        // Exclude rule "wins" for v1 despite order in confing.
        assert!(!matcher.matches("a/exc".try_into()?)?);
        assert!(!matcher.matches("b/exc".try_into()?)?);
        assert!(matcher.matches("a/inc".try_into()?)?);
        assert!(matcher.matches("b/inc".try_into()?)?);

        Ok(())
    }

    #[tokio::test]
    async fn test_matcher_v2() -> anyhow::Result<()> {
        let base = b"
%include child_1
%include child_2

[exclude]
path:a/exc
path:c

[include]
path:a
";

        let child_1 = b"
[include]
path:c

[metadata]
version = 2
";

        let child_2 = b"
[exclude]
path:b/exc
path:c

[include]
path:b

[metadata]
version = 2
";

        let prof = Profile::from_bytes(base, "test".to_string())?;
        let matcher = prof
            .matcher(|path| async move {
                match path.as_ref() {
                    "child_1" => Ok(Some(child_1.to_vec())),
                    "child_2" => Ok(Some(child_2.to_vec())),
                    _ => unreachable!(),
                }
            })
            .await?;

        // Rules directly in root profile still get excludes-go-last ordering.
        assert!(!matcher.matches("a/exc".try_into()?)?);
        assert!(matcher.matches("a/inc".try_into()?)?);

        // Order for v2 child profile is maintained - include rule wins.
        assert!(matcher.matches("b/exc".try_into()?)?);
        assert!(matcher.matches("b/inc".try_into()?)?);

        // "c" is included due to unioning of v2 profiles.
        assert!(matcher.matches("c".try_into()?)?);

        Ok(())
    }

    #[tokio::test]
    async fn test_matcher_missing_include() -> anyhow::Result<()> {
        let config = b"
%include banana
foo
";

        let prof = Profile::from_bytes(config, "test".to_string()).unwrap();

        let matcher = prof.matcher(|_| async { Ok(None) }).await?;

        // We ignore missing includes so that things don't completely
        // break if someone accidentally deletes an in-use sparse
        // profile.
        assert!(matcher.matches("foo".try_into()?)?);

        Ok(())
    }

    #[tokio::test]
    async fn test_explain_empty() {
        let prof = Profile::from_bytes(b"", "test".to_string()).unwrap();
        let matcher = prof
            .matcher(|_| async move { Ok(Some(vec![])) })
            .await
            .unwrap();

        assert_eq!(
            matcher.explain("a/b".try_into().unwrap()).unwrap(),
            (true, "implicit match due to empty profile".to_string())
        );
    }

    #[tokio::test]
    async fn test_explain_no_match() {
        let prof = Profile::from_bytes(b"a", "test".to_string()).unwrap();
        let matcher = prof
            .matcher(|_| async move { Ok(Some(vec![])) })
            .await
            .unwrap();

        assert_eq!(
            matcher.explain("b".try_into().unwrap()).unwrap(),
            (false, "no rules matched".to_string())
        );
    }

    #[tokio::test]
    async fn test_explain_chain() {
        let base = b"%include child_1";
        let child_1 = b"%include child_2";
        let child_2 = b"
[include]
glob:{a,b,c}

[exclude]
path:d
";

        let prof = Profile::from_bytes(base, "base".to_string()).unwrap();
        let matcher = prof
            .matcher(|path| async move {
                match path.as_ref() {
                    "child_1" => Ok(Some(child_1.to_vec())),
                    "child_2" => Ok(Some(child_2.to_vec())),
                    _ => unreachable!(),
                }
            })
            .await
            .unwrap();

        assert_eq!(
            matcher.explain("b".try_into().unwrap()).unwrap(),
            (true, "base -> child_1 -> child_2".to_string())
        );

        assert_eq!(
            matcher.explain("d".try_into().unwrap()).unwrap(),
            (false, "base -> child_1 -> child_2".to_string())
        );
    }

    #[tokio::test]
    async fn test_dynamic_rule_source() {
        let config = b"
one

# source = banana
two
three

# source =
four
";

        let prof = Profile::from_bytes(config, "base".to_string()).unwrap();

        let matcher = prof.matcher(|_| async { Ok(Some(vec![])) }).await.unwrap();

        assert_eq!(
            matcher.explain("one".try_into().unwrap()).unwrap(),
            (true, "base".to_string())
        );

        assert_eq!(
            matcher.explain("two".try_into().unwrap()).unwrap(),
            (true, "base (banana)".to_string())
        );

        assert_eq!(
            matcher.explain("three".try_into().unwrap()).unwrap(),
            (true, "base (banana)".to_string())
        );

        assert_eq!(
            matcher.explain("four".try_into().unwrap()).unwrap(),
            (true, "base".to_string())
        );
    }
}
