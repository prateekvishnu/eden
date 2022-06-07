/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt::{self, Debug, Display};
use std::hash::Hash;
use std::{result, str::FromStr};

use abomonation_derive::Abomonation;
use anyhow::Result;
use async_trait::async_trait;
use blobstore::{Blobstore, Loadable, LoadableError, Storable};
use context::CoreContext;
use edenapi_types::{
    BonsaiChangesetId as EdenapiBonsaiChangesetId, ContentId as EdenapiContentId,
    FsnodeId as EdenapiFsnodeId,
};
use sql::mysql;

use crate::{
    blob::{Blob, BlobstoreValue},
    bonsai_changeset::BonsaiChangeset,
    content_chunk::ContentChunk,
    content_metadata::ContentMetadata,
    deleted_manifest_v2::DeletedManifestV2,
    fastlog_batch::FastlogBatch,
    file_contents::FileContents,
    fsnode::Fsnode,
    hash::{Blake2, Blake2Prefix},
    rawbundle2::RawBundle2,
    redaction_key_list::RedactionKeyList,
    skeleton_manifest::SkeletonManifest,
    thrift,
    unode::{FileUnode, ManifestUnode},
};

// There is no NULL_HASH for typed hashes. Any places that need a null hash should use an
// Option type, or perhaps a list as desired.

/// A type, which can be parsed from a blobstore key,
/// and from which a blobstore key can be produced
/// (this is implemented by various handle types, where
/// blobstore key consists of two things: a hash
/// and a string, describing what the key refers to)
pub trait BlobstoreKey: FromStr<Err = anyhow::Error> {
    /// Return a key suitable for blobstore use.
    fn blobstore_key(&self) -> String;
    fn parse_blobstore_key(key: &str) -> Result<Self>;
}

/// An identifier used throughout Mononoke.
pub trait MononokeId: BlobstoreKey + Debug + Copy + Eq + Hash + Sync + Send + 'static {
    /// Blobstore value type associated with given MononokeId type
    type Value: BlobstoreValue<Key = Self>;

    /// Return a stable hash fingerprint that can be used for sampling
    fn sampling_fingerprint(&self) -> u64;
}

/// An identifier for a changeset in Mononoke.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash, Abomonation)]
#[derive(mysql::OptTryFromRowField)]
pub struct ChangesetId(Blake2);

/// An identifier for a changeset hash prefix in Mononoke.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash, Abomonation)]
pub struct ChangesetIdPrefix(Blake2Prefix);

/// The type for resolving changesets by prefix of the hash
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum ChangesetIdsResolvedFromPrefix {
    /// Found single changeset
    Single(ChangesetId),
    /// Found several changesets within the limit provided
    Multiple(Vec<ChangesetId>),
    /// Found too many changesets exceeding the limit provided
    TooMany(Vec<ChangesetId>),
    /// Changeset was not found
    NoMatch,
}

/// An identifier for file contents in Mononoke.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct ContentId(Blake2);

/// An identifier for a chunk of a file's contents.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct ContentChunkId(Blake2);

/// An identifier for mapping from a ContentId to various aliases for that content
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct ContentMetadataId(Blake2);

/// An identifier for raw bundle2 contents in Mononoke
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct RawBundle2Id(Blake2);

/// An identifier for a file unode
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct FileUnodeId(Blake2);

/// An identifier for a manifest unode
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct ManifestUnodeId(Blake2);

/// An identifier for a deleted manifest v2
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct DeletedManifestV2Id(Blake2);

/// An identifier for a sharded map node used in deleted manifest v2
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct ShardedMapNodeId(Blake2);

/// An identifier for an fsnode
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct FsnodeId(Blake2);

/// An identifier for a skeleton manifest
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct SkeletonManifestId(Blake2);

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct FastlogBatchId(Blake2);

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct BlameId(Blake2);

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct RedactionKeyListId(Blake2);

pub struct Blake2HexVisitor;

impl<'de> serde::de::Visitor<'de> for Blake2HexVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("64 hex digits")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(value.to_string())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(value)
    }
}

/// Implementations of typed hashes.
#[macro_export]
macro_rules! impl_typed_hash_no_context {
    {
        hash_type => $typed: ty,
        thrift_type => $thrift_typed: path,
        blobstore_key => $blobstore_key: expr,
    } => {
        impl $typed {
            pub const fn new(blake2: $crate::private::Blake2) -> Self {
                Self(blake2)
            }

            // (this is public because downstream code wants to be able to deserialize these nodes)
            pub fn from_thrift(h: $thrift_typed) -> $crate::private::anyhow::Result<Self> {
                // This assumes that a null hash is never serialized. This should always be the
                // case.
                match h.0 {
                    $crate::private::thrift::IdType::Blake2(blake2) => Ok(Self::new($crate::private::Blake2::from_thrift(blake2)?)),
                    $crate::private::thrift::IdType::UnknownField(x) => $crate::private::anyhow::bail!($crate::private::ErrorKind::InvalidThrift(
                        stringify!($typed).into(),
                        format!("unknown id type field: {}", x)
                    )),
                }
            }

            #[cfg(test)]
            pub(crate) fn from_byte_array(arr: [u8; 32]) -> Self {
                Self::new($crate::private::Blake2::from_byte_array(arr))
            }

            #[inline]
            pub fn from_bytes(bytes: impl AsRef<[u8]>) -> $crate::private::anyhow::Result<Self> {
                $crate::private::Blake2::from_bytes(bytes).map(Self::new)
            }

            #[inline]
            pub fn from_ascii_str(s: &$crate::private::AsciiStr) -> $crate::private::anyhow::Result<Self> {
                $crate::private::Blake2::from_ascii_str(s).map(Self::new)
            }

            pub fn blake2(&self) -> &$crate::private::Blake2 {
                &self.0
            }

            #[inline]
            pub fn to_hex(&self) -> $crate::private::AsciiString {
                self.0.to_hex()
            }

            pub fn to_brief(&self) -> $crate::private::AsciiString {
                self.to_hex().into_iter().take(8).collect()
            }

            // (this is public because downstream code wants to be able to serialize these nodes)
            pub fn into_thrift(self) -> $thrift_typed {
                $thrift_typed($crate::private::thrift::IdType::Blake2(self.0.into_thrift()))
            }
        }

        impl BlobstoreKey for $typed {
            #[inline]
            fn blobstore_key(&self) -> String {
                format!(concat!($blobstore_key, ".blake2.{}"), self.0)
            }

            fn parse_blobstore_key(key: &str) -> $crate::private::anyhow::Result<Self> {
                let prefix = concat!($blobstore_key, ".blake2.");
                match key.strip_prefix(prefix) {
                    None => $crate::private::anyhow::bail!("{} is not a blobstore key for {}", key, stringify!($typed)),
                    Some(suffix) => Self::from_str(suffix),
                }
            }
        }

        impl TryFrom<$crate::private::Bytes> for $typed {
            type Error = $crate::private::anyhow::Error;
            #[inline]
            fn try_from(b: $crate::private::Bytes) -> $crate::private::anyhow::Result<Self> {
                Self::from_bytes(b)
            }
        }

        impl From<$typed> for $crate::private::Bytes {
            fn from(b: $typed) -> Self {
                Self::copy_from_slice(b.as_ref())
            }
        }

        impl std::str::FromStr for $typed {
            type Err = $crate::private::anyhow::Error;
            #[inline]
            fn from_str(s: &str) -> $crate::private::anyhow::Result<Self> {
                $crate::private::Blake2::from_str(s).map(Self::new)
            }
        }

        impl From<$crate::private::Blake2> for $typed {
            fn from(h: $crate::private::Blake2) -> $typed {
                Self::new(h)
            }
        }

        impl<'a> From<&'a $crate::private::Blake2> for $typed {
            fn from(h: &'a $crate::private::Blake2) -> $typed {
                Self::new(*h)
            }
        }

        impl AsRef<[u8]> for $typed {
            fn as_ref(&self) -> &[u8] {
                self.0.as_ref()
            }
        }

        impl std::fmt::Display for $typed {
            fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, fmt)
            }
        }

        impl $crate::private::Arbitrary for $typed {
            fn arbitrary(g: &mut $crate::private::Gen) -> Self {
                Self::new($crate::private::Blake2::arbitrary(g))
            }

            fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
                $crate::private::empty_shrinker()
            }
        }

        impl $crate::private::Serialize for $typed {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: $crate::private::Serializer,
            {
                serializer.serialize_str(self.to_hex().as_str())
            }
        }

        impl<'de> $crate::private::Deserialize<'de> for $typed {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: $crate::private::Deserializer<'de>,
            {
                use std::str::FromStr;

                let hex = deserializer.deserialize_string($crate::private::Blake2HexVisitor)?;
                match $crate::private::Blake2::from_str(hex.as_str()) {
                    Ok(blake2) => Ok(Self::new(blake2)),
                    Err(error) => Err($crate::private::DeError::custom(error)),
                }
            }
        }

    }
}

macro_rules! impl_typed_hash_loadable_storable {
    {
        hash_type => $typed: ident,
    } => {
        #[async_trait]
        impl Loadable for $typed
        {
            type Value = <$typed as MononokeId>::Value;

            async fn load<'a, B: Blobstore>(
                &'a self,
                ctx: &'a CoreContext,
                blobstore: &'a B,
            ) -> Result<Self::Value, LoadableError> {
                let id = *self;
                let blobstore_key = id.blobstore_key();
                let get = blobstore.get(ctx, &blobstore_key);

                let bytes = get.await?.ok_or(LoadableError::Missing(blobstore_key))?;
                let blob: Blob<$typed> = Blob::new(id, bytes.into_raw_bytes());
                <Self::Value as BlobstoreValue>::from_blob(blob).map_err(LoadableError::Error)
            }
        }

        #[async_trait]
        impl Storable for Blob<$typed>
        {
            type Key = $typed;

            async fn store<'a, B: Blobstore>(
                self,
                ctx: &'a CoreContext,
                blobstore: &'a B,
            ) -> Result<Self::Key> {
                let id = *self.id();
                let bytes = self.into();
                blobstore.put(ctx, id.blobstore_key(), bytes).await?;
                Ok(id)
            }
        }
    }
}

#[macro_export]
macro_rules! impl_typed_context {
    {
        hash_type => $typed: ident,
        context_type => $typed_context: ident,
        context_key => $key: expr,
    } => {
        /// Context for incrementally computing a hash.
        #[derive(Clone)]
        pub struct $typed_context($crate::hash::Context);

        impl $typed_context {
            /// Construct a context.
            #[inline]
            pub fn new() -> Self {
                $typed_context($crate::hash::Context::new($key.as_bytes()))
            }

            #[inline]
            pub fn update<T>(&mut self, data: T)
            where
                T: AsRef<[u8]>,
            {
                self.0.update(data)
            }

            #[inline]
            pub fn finish(self) -> $typed {
                $typed(self.0.finish())
            }
        }

    }
}

macro_rules! impl_typed_hash {
    {
        hash_type => $typed: ident,
        thrift_hash_type => $thrift_hash_type: path,
        value_type => $value_type: ident,
        context_type => $typed_context: ident,
        context_key => $key: expr,
    } => {
        impl_typed_hash_no_context! {
            hash_type => $typed,
            thrift_type => $thrift_hash_type,
            blobstore_key => $key,
        }

        impl_typed_hash_loadable_storable! {
            hash_type => $typed,
        }

        impl_typed_context! {
            hash_type => $typed,
            context_type => $typed_context,
            context_key => $key,
        }

        impl MononokeId for $typed {
            type Value = $value_type;

            #[inline]
            fn sampling_fingerprint(&self) -> u64 {
                self.0.sampling_fingerprint()
            }
        }

    }
}

macro_rules! impl_edenapi_hash_convert {
    ($this: ident, $edenapi: ident) => {
        impl From<$this> for $edenapi {
            fn from(v: $this) -> Self {
                $edenapi::from(v.0.into_inner())
            }
        }

        impl From<$edenapi> for $this {
            fn from(v: $edenapi) -> Self {
                $this::new(Blake2::from_byte_array(v.into()))
            }
        }
    };
}

impl_typed_hash! {
    hash_type => ChangesetId,
    thrift_hash_type => thrift::ChangesetId,
    value_type => BonsaiChangeset,
    context_type => ChangesetIdContext,
    context_key => "changeset",
}

impl_edenapi_hash_convert!(ChangesetId, EdenapiBonsaiChangesetId);

impl_typed_hash! {
    hash_type => ContentId,
    thrift_hash_type => thrift::ContentId,
    value_type => FileContents,
    context_type => ContentIdContext,
    context_key => "content",
}

impl_edenapi_hash_convert!(ContentId, EdenapiContentId);

impl_typed_hash! {
    hash_type => ContentChunkId,
    thrift_hash_type => thrift::ContentChunkId,
    value_type => ContentChunk,
    context_type => ContentChunkIdContext,
    context_key => "chunk",
}

impl_typed_hash! {
    hash_type => RawBundle2Id,
    thrift_hash_type => thrift::RawBundle2Id,
    value_type => RawBundle2,
    context_type => RawBundle2IdContext,
    context_key => "rawbundle2",
}

impl_typed_hash! {
    hash_type => FileUnodeId,
    thrift_hash_type => thrift::FileUnodeId,
    value_type => FileUnode,
    context_type => FileUnodeIdContext,
    context_key => "fileunode",
}

impl_typed_hash! {
    hash_type => ManifestUnodeId,
    thrift_hash_type => thrift::ManifestUnodeId,
    value_type => ManifestUnode,
    context_type => ManifestUnodeIdContext,
    context_key => "manifestunode",
}

impl_typed_hash! {
    hash_type => DeletedManifestV2Id,
    thrift_hash_type => thrift::DeletedManifestV2Id,
    value_type => DeletedManifestV2,
    context_type => DeletedManifestV2Context,
    context_key => "deletedmanifest2",
}

// Manual implementations for ShardedMapNodeId because it has a generic type
// so we can't implement MononokeId for it
impl_typed_hash_no_context! {
    hash_type => ShardedMapNodeId,
    thrift_type => thrift::ShardedMapNodeId,
    // TODO(yancouto): ShardedMapNode shouldn't depend on something that explicitly
    // mentions dfm.
    blobstore_key => "deletedmanifest2.mapnode",
}

impl_typed_context! {
    hash_type => ShardedMapNodeId,
    context_type => ShardedMapNodeContext,
    context_key => "deletedmanifest2.mapnode",
}

impl_typed_hash! {
    hash_type => FsnodeId,
    thrift_hash_type => thrift::FsnodeId,
    value_type => Fsnode,
    context_type => FsnodeIdContext,
    context_key => "fsnode",
}

impl_typed_hash! {
    hash_type => RedactionKeyListId,
    thrift_hash_type => thrift::RedactionKeyListId,
    value_type => RedactionKeyList,
    context_type => RedactionKeyListIdContext,
    context_key => "redactionkeylist",
}

impl_edenapi_hash_convert!(FsnodeId, EdenapiFsnodeId);

impl_typed_hash! {
    hash_type => SkeletonManifestId,
    thrift_hash_type => thrift::SkeletonManifestId,
    value_type => SkeletonManifest,
    context_type => SkeletonManifestIdContext,
    context_key => "skeletonmanifest",
}

impl_typed_hash_no_context! {
    hash_type => ContentMetadataId,
    thrift_type => thrift::ContentMetadataId,
    blobstore_key => "content_metadata",
}

impl_typed_hash_loadable_storable! {
    hash_type => ContentMetadataId,
}

impl_typed_hash! {
    hash_type => FastlogBatchId,
    thrift_hash_type => thrift::FastlogBatchId,
    value_type => FastlogBatch,
    context_type => FastlogBatchIdContext,
    context_key => "fastlogbatch",
}

impl From<ContentId> for ContentMetadataId {
    fn from(content: ContentId) -> Self {
        Self { 0: content.0 }
    }
}

impl MononokeId for ContentMetadataId {
    type Value = ContentMetadata;

    #[inline]
    fn sampling_fingerprint(&self) -> u64 {
        self.0.sampling_fingerprint()
    }
}

impl ChangesetIdPrefix {
    pub const fn new(blake2prefix: Blake2Prefix) -> Self {
        ChangesetIdPrefix(blake2prefix)
    }

    pub fn from_bytes<B: AsRef<[u8]> + ?Sized>(bytes: &B) -> Result<Self> {
        Blake2Prefix::from_bytes(bytes).map(Self::new)
    }

    #[inline]
    pub fn min_as_ref(&self) -> &[u8] {
        self.0.min_as_ref()
    }

    #[inline]
    pub fn max_as_ref(&self) -> &[u8] {
        self.0.max_as_ref()
    }

    #[inline]
    pub fn into_changeset_id(self) -> Option<ChangesetId> {
        self.0.into_blake2().map(ChangesetId)
    }
}

impl FromStr for ChangesetIdPrefix {
    type Err = <Blake2Prefix as FromStr>::Err;
    fn from_str(s: &str) -> result::Result<ChangesetIdPrefix, Self::Err> {
        Blake2Prefix::from_str(s).map(ChangesetIdPrefix)
    }
}

impl Display for ChangesetIdPrefix {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.0, fmt)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use quickcheck::quickcheck;

    quickcheck! {
        fn changesetid_thrift_roundtrip(h: ChangesetId) -> bool {
            let v = h.into_thrift();
            let sh = ChangesetId::from_thrift(v)
                .expect("converting a valid Thrift structure should always work");
            h == sh
        }

        fn contentid_thrift_roundtrip(h: ContentId) -> bool {
            let v = h.into_thrift();
            let sh = ContentId::from_thrift(v)
                .expect("converting a valid Thrift structure should always work");
            h == sh
        }
    }

    #[test]
    fn blobstore_key() {
        // These IDs are persistent, and this test is really to make sure that they don't change
        // accidentally.
        let id = ChangesetId::new(Blake2::from_byte_array([1; 32]));
        assert_eq!(id.blobstore_key(), format!("changeset.blake2.{}", id));

        let id = ContentId::new(Blake2::from_byte_array([1; 32]));
        assert_eq!(id.blobstore_key(), format!("content.blake2.{}", id));

        let id = ShardedMapNodeId::from_byte_array([1; 32]);
        assert_eq!(
            id.blobstore_key(),
            format!("deletedmanifest2.mapnode.blake2.{}", id)
        );

        let id = ContentChunkId::from_byte_array([1; 32]);
        assert_eq!(id.blobstore_key(), format!("chunk.blake2.{}", id));

        let id = RawBundle2Id::from_byte_array([1; 32]);
        assert_eq!(id.blobstore_key(), format!("rawbundle2.blake2.{}", id));

        let id = FileUnodeId::from_byte_array([1; 32]);
        assert_eq!(id.blobstore_key(), format!("fileunode.blake2.{}", id));

        let id = ManifestUnodeId::from_byte_array([1; 32]);
        assert_eq!(id.blobstore_key(), format!("manifestunode.blake2.{}", id));

        let id = DeletedManifestV2Id::from_byte_array([1; 32]);
        assert_eq!(
            id.blobstore_key(),
            format!("deletedmanifest2.blake2.{}", id)
        );

        let id = FsnodeId::from_byte_array([1; 32]);
        assert_eq!(id.blobstore_key(), format!("fsnode.blake2.{}", id));

        let id = SkeletonManifestId::from_byte_array([1; 32]);
        assert_eq!(
            id.blobstore_key(),
            format!("skeletonmanifest.blake2.{}", id)
        );

        let id = ContentMetadataId::from_byte_array([1; 32]);
        assert_eq!(
            id.blobstore_key(),
            format!("content_metadata.blake2.{}", id)
        );

        let id = FastlogBatchId::from_byte_array([1; 32]);
        assert_eq!(id.blobstore_key(), format!("fastlogbatch.blake2.{}", id));

        let id = RedactionKeyListId::from_byte_array([1; 32]);
        assert_eq!(
            id.blobstore_key(),
            format!("redactionkeylist.blake2.{}", id)
        );
    }

    #[test]
    fn test_serialize_deserialize() {
        let id = ChangesetId::new(Blake2::from_byte_array([1; 32]));
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        let id = ContentId::new(Blake2::from_byte_array([1; 32]));
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        let id = ShardedMapNodeId::from_byte_array([1; 32]);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        let id = ContentChunkId::from_byte_array([1; 32]);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        let id = RawBundle2Id::from_byte_array([1; 32]);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        let id = FileUnodeId::from_byte_array([1; 32]);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        let id = ManifestUnodeId::from_byte_array([1; 32]);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        let id = DeletedManifestV2Id::from_byte_array([1; 32]);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        let id = FsnodeId::from_byte_array([1; 32]);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        let id = SkeletonManifestId::from_byte_array([1; 32]);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        let id = ContentMetadataId::from_byte_array([1; 32]);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        let id = FastlogBatchId::from_byte_array([1; 32]);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        let id = RedactionKeyListId::from_byte_array([1; 32]);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);
    }
}
