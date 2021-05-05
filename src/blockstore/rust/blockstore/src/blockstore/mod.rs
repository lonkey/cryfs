use anyhow::Result;
use std::marker::PhantomData;
use std::ops::Add;
use typenum::Unsigned;

use crate::data::{Data, GrowableData};

pub use cppbridge::{BlockId, BLOCKID_LEN};

pub trait BlockStoreReader {
    fn load(&self, id: &BlockId) -> Result<Option<Data>>;
    fn num_blocks(&self) -> Result<u64>;
    fn estimate_num_free_bytes(&self) -> Result<u64>;
    fn block_size_from_physical_block_size(&self, block_size: u64) -> Result<u64>;

    fn all_blocks(&self) -> Result<Box<dyn Iterator<Item = BlockId>>>;
}

pub trait BlockStoreDeleter {
    fn remove(&self, id: &BlockId) -> Result<bool>;
}

pub trait BlockStoreWriter {
    fn try_create(&self, id: &BlockId, data: &[u8]) -> Result<bool>;
    fn store(&self, id: &BlockId, data: &[u8]) -> Result<()>;
}

pub trait OptimizedBlockStoreWriterMetadata {
    type RequiredPrefixBytesBase: Unsigned;
    type RequiredPrefixBytesSelf: Unsigned;
}

pub trait OptimizedBlockStoreWriter: OptimizedBlockStoreWriterMetadata
where
    Self::RequiredPrefixBytesBase: Add<Self::RequiredPrefixBytesSelf>,
{
    /// In-memory representation of the data of a block. This can be allocated using [OptimizedBlockStoreWriter::allocate]
    /// and then can be passed to [OptimizedBlockStoreWriter::try_create_optimized] or [OptimizedBlockStoreWriter::store_optimized].
    ///
    /// The reason we use this class and don't use just [crate::data::Data] or `&[u8]` is for optimizations purposes.
    /// Some blockstores prepend header to the data before storing and require the block data to be set up in a way
    /// that makes sure that data can be prepended without having to copy the block data.
    // type BlockData: block_data::IBlockData;

    /// Allocates an in-memory representation of a data block that can be written to
    /// and that can then be passed to [OptimizedBlockStoreWriter::try_create_optimized] or [OptimizedBlockStoreWriter::store_optimized].
    fn allocate(
        size: usize,
    ) -> GrowableData<
        <Self::RequiredPrefixBytesBase as Add<Self::RequiredPrefixBytesSelf>>::Output,
        typenum::U0,
    >;

    fn try_create_optimized(
        &self,
        id: &BlockId,
        data: GrowableData<
            <Self::RequiredPrefixBytesBase as Add<Self::RequiredPrefixBytesSelf>>::Output,
            typenum::U0,
        >,
    ) -> Result<bool>;
    fn store_optimized(
        &self,
        id: &BlockId,
        data: GrowableData<
            <Self::RequiredPrefixBytesBase as Add<Self::RequiredPrefixBytesSelf>>::Output,
            typenum::U0,
        >,
    ) -> Result<()>;
}

impl<B: OptimizedBlockStoreWriter> BlockStoreWriter for B
where
    Self: OptimizedBlockStoreWriterMetadata,
    <Self as OptimizedBlockStoreWriterMetadata>::RequiredPrefixBytesBase:
        Add<<Self as OptimizedBlockStoreWriterMetadata>::RequiredPrefixBytesSelf>,
{
    fn try_create(&self, id: &BlockId, data: &[u8]) -> Result<bool> {
        let mut block_data = Self::allocate(data.len());
        assert_eq!(block_data.as_ref().len(), data.len());
        block_data.as_mut().copy_from_slice(data);
        self.try_create_optimized(id, block_data)
    }

    fn store(&self, id: &BlockId, data: &[u8]) -> Result<()> {
        let mut block_data = Self::allocate(data.len());
        assert_eq!(block_data.as_ref().len(), data.len());
        block_data.as_mut().copy_from_slice(data);
        self.store_optimized(id, block_data)
    }
}

pub trait BlockStore: BlockStoreReader + BlockStoreWriter + BlockStoreDeleter
where
    Self: OptimizedBlockStoreWriterMetadata,
    <Self as OptimizedBlockStoreWriterMetadata>::RequiredPrefixBytesBase:
        Add<<Self as OptimizedBlockStoreWriterMetadata>::RequiredPrefixBytesSelf>,
{
}

/// BlockData instances wrap a [Data] instance and guarantee the upholding of an
/// important invariant for [OptimizedBlockStoreWriter], namely that the data stored
/// has enough prefix bytes available and can be grown during the writing process
/// to e.g. add a block header without requiring the block data to be copied.
/// Such BlockData instances can be created with the [create_block_data_wrapper!] macro.
///
/// This not being public is an important part of our safety net.
/// Only things in the blockstore module can create instances of this,
/// so we can make sure the invariants are always kept.
#[macro_use]
mod block_data {
    use super::Data;

    pub trait IBlockData: AsRef<[u8]> + AsMut<[u8]> {
        fn new(data: Data) -> Self;
        fn extract(self) -> Data;
    }

    macro_rules! create_block_data_wrapper {
        ($name: ident) => {
            pub struct $name(Data);

            impl AsRef<[u8]> for BlockData {
                fn as_ref(&self) -> &[u8] {
                    self.0.as_ref()
                }
            }

            impl AsMut<[u8]> for BlockData {
                fn as_mut(&mut self) -> &mut [u8] {
                    self.0.as_mut()
                }
            }

            impl crate::blockstore::block_data::IBlockData for $name {
                fn new(data: Data) -> Self {
                    Self(data)
                }

                fn extract(self) -> Data {
                    self.0
                }
            }
        };
    }
}

mod cppbridge;
mod encrypted;
mod inmemory;
mod integrity;
mod ondisk;
