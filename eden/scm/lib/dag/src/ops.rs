/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! DAG and Id operations (mostly traits)

use crate::default_impl;
use crate::id::Group;
use crate::id::Id;
use crate::id::VertexName;
use crate::namedag::MemNameDag;
use crate::nameset::NameSet;
use anyhow::Result;
use std::collections::HashMap;

/// DAG related read-only algorithms.
pub trait DagAlgorithm {
    /// Sort a `NameSet` topologically.
    fn sort(&self, set: &NameSet) -> Result<NameSet>;

    /// Re-create the graph so it looks better when rendered.
    ///
    /// For example, the left-side graph will be rewritten to the right-side:
    ///
    /// 1. Linearize.
    ///
    /// ```plain,ignore
    ///   A             A      # Linearize is done by IdMap::assign_heads,
    ///   |             |      # as long as the heads provided are the heads
    ///   | C           B      # of the whole graph ("A", "C", not "B", "D").
    ///   | |           |
    ///   B |     ->    | C
    ///   | |           | |
    ///   | D           | D
    ///   |/            |/
    ///   E             E
    /// ```
    ///
    /// 2. Reorder branches (at different branching points) to reduce columns.
    ///
    /// ```plain,ignore
    ///     D           B
    ///     |           |      # Assuming the main branch is B-C-E.
    ///   B |           | A    # Branching point of the D branch is "C"
    ///   | |           |/     # Branching point of the A branch is "C"
    ///   | | A   ->    C      # The D branch should be moved to below
    ///   | |/          |      # the A branch.
    ///   | |           | D
    ///   |/|           |/
    ///   C /           E
    ///   |/
    ///   E
    /// ```
    ///
    /// 3. Reorder branches (at a same branching point) to reduce length of
    ///    edges.
    ///
    /// ```plain,ignore
    ///   D              A
    ///   |              |     # This is done by picking the longest
    ///   | A            B     # branch (A-B-C-E) as the "main branch"
    ///   | |            |     # and work on the remaining branches
    ///   | B     ->     C     # recursively.
    ///   | |            |
    ///   | C            | D
    ///   |/             |/
    ///   E              E
    /// ```
    ///
    /// `main_branch` optionally defines how to sort the heads. A head `x` will
    /// be emitted first during iteration, if `ancestors(x) & main_branch`
    /// contains larger vertexes. For example, if `main_branch` is `[C, D, E]`,
    /// then `C` will be emitted first, and the returned DAG will have `all()`
    /// output `[C, D, A, B, E]`. Practically, `main_branch` usually contains
    /// "public" commits.
    ///
    /// This function is expensive. Only run on small graphs.
    ///
    /// This function is currently more optimized for "forking" cases. It is
    /// not yet optimized for graphs with many merges.
    fn beautify(&self, main_branch: Option<NameSet>) -> Result<MemNameDag> {
        // Find the "largest" branch.
        fn find_main_branch(
            get_ancestors: &impl Fn(&VertexName) -> Result<NameSet>,
            heads: &[VertexName],
        ) -> Result<NameSet> {
            let mut best_branch = NameSet::empty();
            let mut best_count = best_branch.count()?;
            for head in heads {
                let branch = get_ancestors(head)?;
                let count = branch.count()?;
                if count > best_count {
                    best_count = count;
                    best_branch = branch;
                }
            }
            Ok(best_branch)
        };

        // Sort heads recursively.
        fn sort(
            get_ancestors: &impl Fn(&VertexName) -> Result<NameSet>,
            heads: &mut [VertexName],
            main_branch: NameSet,
        ) -> Result<()> {
            if heads.len() <= 1 {
                return Ok(());
            }

            // Sort heads by "branching point" on the main branch.
            let mut branching_points: HashMap<VertexName, usize> =
                HashMap::with_capacity(heads.len());
            for head in heads.iter() {
                let count = (get_ancestors(head)? & main_branch.clone()).count()?;
                branching_points.insert(head.clone(), count);
            }
            heads.sort_by_key(|v| branching_points.get(v));

            // For heads with a same branching point, sort them recursively
            // using a different "main branch".
            let mut start = 0;
            let mut start_branching_point: Option<usize> = None;
            for end in 0..=heads.len() {
                let branching_point = heads
                    .get(end)
                    .and_then(|h| branching_points.get(&h).cloned());
                if branching_point != start_branching_point {
                    if start + 1 < end {
                        let heads = &mut heads[start..end];
                        let main_branch = find_main_branch(get_ancestors, heads)?;
                        sort(get_ancestors, heads, main_branch)?;
                    }
                    start = end;
                    start_branching_point = branching_point;
                }
            }

            Ok(())
        };

        let main_branch = main_branch.unwrap_or_else(|| NameSet::empty());
        let mut heads: Vec<_> = self
            .heads_ancestors(self.all()?)?
            .iter()?
            .collect::<Result<_>>()?;
        let get_ancestors = |head: &VertexName| self.ancestors(head.into());
        // Stabilize output if the sort key conflicts.
        heads.sort();
        sort(&get_ancestors, &mut heads[..], main_branch)?;

        let mut dag = MemNameDag::new();
        let get_parents = |v| self.parent_names(v);
        dag.add_heads(get_parents, &heads)?;
        Ok(dag)
    }

    /// Get ordered parent vertexes.
    fn parent_names(&self, name: VertexName) -> Result<Vec<VertexName>>;

    /// Returns a [`SpanSet`] that covers all vertexes tracked by this DAG.
    fn all(&self) -> Result<NameSet>;

    /// Calculates all ancestors reachable from any name from the given set.
    fn ancestors(&self, set: NameSet) -> Result<NameSet>;

    /// Calculates parents of the given set.
    ///
    /// Note: Parent order is not preserved. Use [`NameDag::parent_names`]
    /// to preserve order.
    fn parents(&self, set: NameSet) -> Result<NameSet> {
        default_impl::parents(self, set)
    }

    /// Calculates the n-th first ancestor.
    fn first_ancestor_nth(&self, name: VertexName, n: u64) -> Result<VertexName> {
        default_impl::first_ancestor_nth(self, name, n)
    }

    /// Calculates heads of the given set.
    fn heads(&self, set: NameSet) -> Result<NameSet> {
        default_impl::heads(self, set)
    }

    /// Calculates children of the given set.
    fn children(&self, set: NameSet) -> Result<NameSet>;

    /// Calculates roots of the given set.
    fn roots(&self, set: NameSet) -> Result<NameSet> {
        default_impl::roots(self, set)
    }

    /// Calculates one "greatest common ancestor" of the given set.
    ///
    /// If there are no common ancestors, return None.
    /// If there are multiple greatest common ancestors, pick one arbitrarily.
    /// Use `gca_all` to get all of them.
    fn gca_one(&self, set: NameSet) -> Result<Option<VertexName>> {
        default_impl::gca_one(self, set)
    }

    /// Calculates all "greatest common ancestor"s of the given set.
    /// `gca_one` is faster if an arbitrary answer is ok.
    fn gca_all(&self, set: NameSet) -> Result<NameSet> {
        default_impl::gca_all(self, set)
    }

    /// Calculates all common ancestors of the given set.
    fn common_ancestors(&self, set: NameSet) -> Result<NameSet> {
        default_impl::common_ancestors(self, set)
    }

    /// Tests if `ancestor` is an ancestor of `descendant`.
    fn is_ancestor(&self, ancestor: VertexName, descendant: VertexName) -> Result<bool> {
        default_impl::is_ancestor(self, ancestor, descendant)
    }

    /// Calculates "heads" of the ancestors of the given set. That is,
    /// Find Y, which is the smallest subset of set X, where `ancestors(Y)` is
    /// `ancestors(X)`.
    ///
    /// This is faster than calculating `heads(ancestors(set))` in certain
    /// implementations like segmented changelog.
    ///
    /// This is different from `heads`. In case set contains X and Y, and Y is
    /// an ancestor of X, but not the immediate ancestor, `heads` will include
    /// Y while this function won't.
    fn heads_ancestors(&self, set: NameSet) -> Result<NameSet> {
        default_impl::heads_ancestors(self, set)
    }

    /// Calculates the "dag range" - vertexes reachable from both sides.
    fn range(&self, roots: NameSet, heads: NameSet) -> Result<NameSet>;

    /// Calculates the descendants of the given set.
    fn descendants(&self, set: NameSet) -> Result<NameSet>;
}

/// Add vertexes recursively to the DAG.
pub trait DagAddHeads {
    /// Add vertexes and their ancestors to the DAG. This does not persistent
    /// changes to disk.
    fn add_heads<F>(&mut self, parents: F, heads: &[VertexName]) -> Result<()>
    where
        F: Fn(VertexName) -> Result<Vec<VertexName>>;
}

/// Persistent the DAG on disk.
pub trait DagPersistent {
    /// Write in-memory DAG to disk. This might also pick up changes to
    /// the DAG by other processes.
    fn flush(&mut self, master_heads: &[VertexName]) -> Result<()>;

    /// A faster path for add_heads, followed by flush.
    fn add_heads_and_flush<F>(
        &mut self,
        parent_names_func: F,
        master_names: &[VertexName],
        non_master_names: &[VertexName],
    ) -> Result<()>
    where
        F: Fn(VertexName) -> Result<Vec<VertexName>>;
}

/// Import ASCII graph to DAG.
pub trait ImportAscii {
    /// Import vertexes described in an ASCII graph.
    /// `heads` optionally specifies the order of heads to insert.
    /// Useful for testing. Panic if the input is invalid.
    fn import_ascii_with_heads(
        &mut self,
        text: &str,
        heads: Option<&[impl AsRef<str>]>,
    ) -> Result<()>;

    /// Import vertexes described in an ASCII graph.
    fn import_ascii(&mut self, text: &str) -> Result<()> {
        self.import_ascii_with_heads(text, <Option<&[&str]>>::None)
    }
}

/// Lookup vertexes by prefixes.
pub trait PrefixLookup {
    /// Lookup vertexes by hex prefix.
    fn vertexes_by_hex_prefix(&self, hex_prefix: &[u8], limit: usize) -> Result<Vec<VertexName>>;
}

/// Convert between `Vertex` and `Id`.
pub trait IdConvert {
    fn vertex_id(&self, name: VertexName) -> Result<Id>;
    fn vertex_id_with_max_group(&self, name: &VertexName, max_group: Group) -> Result<Option<Id>>;
    fn vertex_name(&self, id: Id) -> Result<VertexName>;
    fn contains_vertex_name(&self, name: &VertexName) -> Result<bool>;
}

impl<T> ImportAscii for T
where
    T: DagAddHeads,
{
    fn import_ascii_with_heads(
        &mut self,
        text: &str,
        heads: Option<&[impl AsRef<str>]>,
    ) -> Result<()> {
        let parents = drawdag::parse(&text);
        let heads: Vec<_> = match heads {
            Some(heads) => heads
                .iter()
                .map(|s| VertexName::copy_from(s.as_ref().as_bytes()))
                .collect(),
            None => {
                let mut heads: Vec<_> = parents
                    .keys()
                    .map(|s| VertexName::copy_from(s.as_bytes()))
                    .collect();
                heads.sort();
                heads
            }
        };

        let parents_func = move |name: VertexName| -> Result<Vec<VertexName>> {
            Ok(parents[&String::from_utf8(name.as_ref().to_vec()).unwrap()]
                .iter()
                .map(|p| VertexName::copy_from(p.as_bytes()))
                .collect())
        };
        self.add_heads(&parents_func, &heads[..])?;
        Ok(())
    }
}
