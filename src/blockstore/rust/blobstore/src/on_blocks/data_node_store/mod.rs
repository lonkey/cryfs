use anyhow::{anyhow, bail, ensure, Result};
use async_trait::async_trait;
use binary_layout::Field;
#[cfg(test)]
use futures::Stream;
#[cfg(test)]
use std::pin::Pin;

pub use crate::RemoveResult;
#[cfg(test)]
use cryfs_blockstore::TryCreateResult;
use cryfs_blockstore::{BlockId, BlockStore, LockingBlockStore, BLOCKID_LEN};
use cryfs_utils::{
    async_drop::{AsyncDrop, AsyncDropGuard},
    data::Data,
};

mod layout;
use layout::node;
pub use layout::NodeLayout;

mod data_node;
pub use data_node::{DataInnerNode, DataLeafNode, DataNode};

#[cfg(test)]
mod testutils;

#[cfg(test)]
mod test_as_blockstore;

#[derive(Debug)]
pub struct DataNodeStore<B: BlockStore + Send + Sync> {
    block_store: AsyncDropGuard<LockingBlockStore<B>>,
    layout: NodeLayout,
}

impl<B: BlockStore + Send + Sync> DataNodeStore<B> {
    pub async fn new(
        mut block_store: AsyncDropGuard<LockingBlockStore<B>>,
        physical_block_size_bytes: u32,
    ) -> Result<AsyncDropGuard<Self>> {
        let block_size_bytes =
            match Self::_block_size_bytes(&block_store, physical_block_size_bytes) {
                Ok(ok) => ok,
                Err(err) => {
                    block_store.async_drop().await?;
                    return Err(err);
                }
            };

        Ok(AsyncDropGuard::new(Self {
            block_store,
            layout: NodeLayout { block_size_bytes },
        }))
    }

    fn _block_size_bytes(
        block_store: &LockingBlockStore<B>,
        physical_block_size_bytes: u32,
    ) -> Result<u32> {
        let block_size_bytes = block_store
            .block_size_from_physical_block_size(u64::from(physical_block_size_bytes))?;
        let block_size_bytes = u32::try_from(block_size_bytes).unwrap();

        // Min block size: enough for header and for inner nodes to have at least two children and form a tree.
        let min_block_size = u32::try_from(node::data::OFFSET + 2 * BLOCKID_LEN).unwrap();
        ensure!(
            block_size_bytes >= min_block_size,
            "Tried to create a DataNodeStore with block size {} (physical: {}) but must be at least {}",
            block_size_bytes,
            physical_block_size_bytes,
            min_block_size,
        );
        Ok(block_size_bytes)
    }

    pub fn layout(&self) -> &NodeLayout {
        &self.layout
    }

    pub async fn load(&self, block_id: BlockId) -> Result<Option<DataNode<B>>> {
        match self.block_store.load(block_id).await? {
            None => Ok(None),
            Some(block) => DataNode::parse(block, &self.layout).map(Some),
        }
    }

    fn _allocate_data_for_leaf_node(&self) -> Data {
        let mut data = Data::from(vec![
            0;
            usize::try_from(self.layout.block_size_bytes).unwrap()
        ]);
        data.shrink_to_subregion(node::data::OFFSET..);
        assert_eq!(
            usize::try_from(self.layout.max_bytes_per_leaf()).unwrap(),
            data.len()
        );
        data
    }

    pub async fn create_new_leaf_node(&self, data: &Data) -> Result<DataLeafNode<B>> {
        let block_data = self._serialize_leaf(data);
        // TODO Use create_optimized instead of create?
        let block_id = self.block_store.create(&block_data).await?;
        // TODO Avoid extra load here. Do our callers actually need this object? If no, just return the block id. If yes, maybe change block store API to return the block?
        self._load_created_node(block_id).await
    }

    #[cfg(test)]
    pub async fn try_create_new_leaf_node(
        &self,
        block_id: BlockId,
        data: &Data,
    ) -> Result<DataLeafNode<B>> {
        let block_data = self._serialize_leaf(data);
        // TODO Use create_optimized instead of create?
        match self.block_store.try_create(&block_id, &block_data).await? {
            TryCreateResult::SuccessfullyCreated => {}
            TryCreateResult::NotCreatedBecauseBlockIdAlreadyExists => bail!("Block already exists"),
        }
        // TODO Avoid extra load here. Do our callers actually need this object? If no, just return the block id. If yes, maybe change block store API to return the block?
        self._load_created_node(block_id).await
    }

    fn _serialize_leaf(&self, data: &Data) -> Data {
        let mut leaf_data = self._allocate_data_for_leaf_node();
        leaf_data[..data.len()].copy_from_slice(data); // TODO Avoid copy_from_slice and instead rename this function to create_new_leaf_node_optimized
        data_node::serialize_leaf_node_optimized(
            leaf_data,
            u32::try_from(data.len()).unwrap(),
            &self.layout,
        )
    }

    async fn _load_created_node(&self, block_id: BlockId) -> Result<DataLeafNode<B>> {
        match self.load(block_id).await? {
            None => bail!("We just created this block, it must exist"),
            Some(DataNode::Inner(_)) => {
                bail!("We just created a leaf node but then it got loaded as an inner node")
            }
            Some(DataNode::Leaf(node)) => Ok(node),
        }
    }

    pub async fn create_new_inner_node(
        &self,
        depth: u8,
        children: &[BlockId],
    ) -> Result<DataInnerNode<B>> {
        let block_data = data_node::serialize_inner_node(depth, children, &self.layout);
        // TODO Use create_optimized instead of create?
        let blockid = self.block_store.create(&block_data).await?;
        // TODO Avoid extra load here. Do our callers actually need this object? If no, just return the block id. If yes, maybe change block store API to return the block?
        match self.load(blockid).await? {
            None => bail!("We just created this block, it must exist"),
            Some(DataNode::Leaf(_)) => {
                bail!("We just created a inner node but then it got loaded as a leaf node")
            }
            Some(DataNode::Inner(node)) => Ok(node),
        }
    }

    pub async fn create_new_node_as_copy_from(&self, source: &DataNode<B>) -> Result<DataNode<B>> {
        let source_data = source.raw_blockdata();
        assert_eq!(usize::try_from(self.layout.block_size_bytes).unwrap(), source_data.len(), "Source node has wrong layout and has {} bytes. We expected {} bytes. Is it from the same DataNodeStore?", source_data.len(), self.layout.block_size_bytes);
        // TODO Use create_optimized instead of create?
        let blockid = self.block_store.create(source_data).await?;
        // TODO Avoid extra load here. Do our callers actually need this object? If no, just return the block id. If yes, maybe change block store API to return the block?
        self.load(blockid)
            .await?
            .ok_or_else(|| anyhow!("We just created {:?} but now couldn't find it", blockid))
    }

    pub async fn overwrite_leaf_node(&self, block_id: &BlockId, data: &[u8]) -> Result<()> {
        let mut data_obj = self._allocate_data_for_leaf_node();
        // TODO Make an overwrite_leaf_node_optimized version that requires that enough prefix bytes are already available in the data input and that doesn't require us to copy_from_slice here?
        (&mut data_obj.as_mut()[..data.len()]).copy_from_slice(data);
        let block_data = data_node::serialize_leaf_node_optimized(
            data_obj,
            u32::try_from(data.len()).unwrap(),
            &self.layout,
        );
        // TODO Use store_optimized instead of store?
        self.block_store.overwrite(block_id, &block_data).await
    }

    pub async fn remove_by_id(&self, block_id: &BlockId) -> Result<RemoveResult> {
        self.block_store.remove(block_id).await
    }

    pub async fn num_nodes(&self) -> Result<u64> {
        self.block_store.num_blocks().await
    }

    pub fn estimate_space_for_num_blocks_left(&self) -> Result<u64> {
        Ok(self.block_store.estimate_num_free_bytes()?
            / u64::from(self.layout.max_bytes_per_leaf()))
    }

    pub fn virtual_block_size_bytes(&self) -> u32 {
        self.layout.max_bytes_per_leaf()
    }

    pub async fn flush_node(&self, node: &mut DataNode<B>) -> Result<()> {
        self.block_store.flush_block(node.as_block_mut()).await
    }

    #[cfg(test)]
    pub async fn all_nodes(&self) -> Result<Pin<Box<dyn Stream<Item = Result<BlockId>> + Send>>> {
        self.block_store.all_blocks().await
    }

    #[cfg(test)]
    pub async fn clear_cache_slow(&self) -> Result<()> {
        self.block_store.clear_cache_slow().await
    }
}

#[async_trait]
impl<B: BlockStore + Send + Sync> AsyncDrop for DataNodeStore<B> {
    type Error = anyhow::Error;

    async fn async_drop_impl(&mut self) -> Result<(), Self::Error> {
        self.block_store.async_drop().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cryfs_blockstore::InMemoryBlockStore;
    use testutils::*;

    mod create_new_leaf_node {
        use super::*;

        async fn test(nodestore: &DataNodeStore<InMemoryBlockStore>, data: Data) {
            let node = nodestore.create_new_leaf_node(&data).await.unwrap();
            assert_eq!(data.as_ref(), node.data());

            // and it's still correct after loading
            let block_id = *node.block_id();
            drop(node);
            let node = load_leaf_node(nodestore, block_id).await;
            assert_eq!(data.as_ref(), node.data());
        }

        #[tokio::test]
        async fn empty() {
            with_nodestore(move |nodestore| {
                Box::pin(async move { test(nodestore, Data::empty()).await })
            })
            .await
        }

        #[tokio::test]
        async fn some_data() {
            with_nodestore(move |nodestore| {
                Box::pin(async move { test(nodestore, half_full_leaf_data(1)).await })
            })
            .await
        }

        #[tokio::test]
        async fn full() {
            with_nodestore(move |nodestore| {
                Box::pin(async move { test(nodestore, full_leaf_data(1)).await })
            })
            .await
        }

        #[tokio::test]
        #[should_panic = "range end index 1017 out of range for slice of length 1016"]
        async fn too_large_leaf_fails() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let data = full_leaf_data(1);
                    let mut data = data.as_ref().to_vec();
                    data.push(0);
                    let data = Data::from(data);
                    let _ = nodestore.create_new_leaf_node(&data).await;
                })
            })
            .await
        }
    }

    mod create_new_inner_node {
        use futures::future;

        use super::*;

        async fn test(
            nodestore: &DataNodeStore<InMemoryBlockStore>,
            depth: u8,
            children: &[BlockId],
        ) {
            let node = nodestore
                .create_new_inner_node(depth, children)
                .await
                .unwrap();
            assert_eq!(depth, node.depth().get());
            assert_eq!(children.len(), node.num_children().get() as usize);
            assert_eq!(children, &node.children().collect::<Vec<_>>());

            // and it's still correct after loading
            let block_id = *node.block_id();
            drop(node);
            let node = load_inner_node(nodestore, block_id).await;
            assert_eq!(depth, node.depth().get());
            assert_eq!(children.len(), node.num_children().get() as usize);
            assert_eq!(children, &node.children().collect::<Vec<_>>());
        }

        #[tokio::test]
        async fn one_child_leaf() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let child = *new_full_leaf_node(nodestore).await.block_id();
                    test(nodestore, 1, &[child]).await
                })
            })
            .await
        }

        #[tokio::test]
        async fn two_children_leaves() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let child1 = *new_full_leaf_node(nodestore).await.block_id();
                    let child2 = *new_full_leaf_node(nodestore).await.block_id();
                    test(nodestore, 1, &[child1, child2]).await
                })
            })
            .await
        }

        #[tokio::test]
        async fn max_children_leaves() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let children = new_full_leaves(
                        nodestore,
                        nodestore.layout().max_children_per_inner_node(),
                    )
                    .await;
                    test(nodestore, 1, &children).await
                })
            })
            .await
        }

        #[tokio::test]
        async fn one_child_inner() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let child = *new_inner_node(nodestore).await.block_id();
                    test(nodestore, 1, &[child]).await
                })
            })
            .await
        }

        #[tokio::test]
        async fn two_children_inner() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let child1 = *new_inner_node(nodestore).await.block_id();
                    let child2 = *new_inner_node(nodestore).await.block_id();
                    test(nodestore, 1, &[child1, child2]).await
                })
            })
            .await
        }

        #[tokio::test]
        async fn max_children_inner() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let children = future::join_all(
                        (0..nodestore.layout().max_children_per_inner_node())
                            .map(|_| async { *new_inner_node(nodestore).await.block_id() })
                            .collect::<Vec<_>>(),
                    )
                    .await;
                    test(nodestore, 1, &children).await
                })
            })
            .await
        }

        #[tokio::test]
        #[should_panic = "Inner node cannot have a depth of 0. Is this perhaps a leaf instead?"]
        async fn depth0_fails() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let child = *new_full_leaf_node(nodestore).await.block_id();
                    let _ = nodestore.create_new_inner_node(0, &[child]).await;
                })
            })
            .await
        }
    }

    mod create_new_node_as_copy_from {
        use super::*;

        #[tokio::test]
        async fn empty_leaf() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let node = new_empty_leaf_node(nodestore).await.upcast();
                    let copy = nodestore.create_new_node_as_copy_from(&node).await.unwrap();
                    assert_ne!(node.block_id(), copy.block_id());
                    assert_eq!(node.raw_blockdata(), copy.raw_blockdata());
                    let node_id = *node.block_id();
                    let copy_id = *copy.block_id();

                    //And data is correct after loading
                    drop(node);
                    drop(copy);
                    let node = load_leaf_node(nodestore, node_id).await;
                    let copy = load_leaf_node(nodestore, copy_id).await;
                    assert_eq!(0, node.num_bytes());
                    assert_eq!(&[0u8; 0], node.data());
                    assert_eq!(0, copy.num_bytes());
                    assert_eq!(&[0u8; 0], copy.data());
                })
            })
            .await
        }

        #[tokio::test]
        async fn half_full_leaf() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let len = nodestore.layout().max_bytes_per_leaf() as usize / 2;
                    let node = nodestore
                        .create_new_leaf_node(&data_fixture(len, 1))
                        .await
                        .unwrap()
                        .upcast();
                    let copy = nodestore.create_new_node_as_copy_from(&node).await.unwrap();
                    assert_ne!(node.block_id(), copy.block_id());
                    assert_eq!(node.raw_blockdata(), copy.raw_blockdata());
                    let node_id = *node.block_id();
                    let copy_id = *copy.block_id();

                    // And data is correct after loading
                    drop(node);
                    drop(copy);
                    let node = load_leaf_node(nodestore, node_id).await;
                    let copy = load_leaf_node(nodestore, copy_id).await;
                    assert_eq!(len, node.num_bytes() as usize);
                    assert_eq!(data_fixture(len, 1).as_ref(), node.data());
                    assert_eq!(len, copy.num_bytes() as usize);
                    assert_eq!(data_fixture(len, 1).as_ref(), copy.data());
                })
            })
            .await
        }

        #[tokio::test]
        async fn full_leaf() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let len = nodestore.layout().max_bytes_per_leaf() as usize;
                    let node = nodestore
                        .create_new_leaf_node(&data_fixture(len, 1))
                        .await
                        .unwrap()
                        .upcast();
                    let copy = nodestore.create_new_node_as_copy_from(&node).await.unwrap();
                    assert_ne!(node.block_id(), copy.block_id());
                    assert_eq!(node.raw_blockdata(), copy.raw_blockdata());
                    let node_id = *node.block_id();
                    let copy_id = *copy.block_id();

                    // And data is correct after loading
                    drop(node);
                    drop(copy);
                    let node = load_leaf_node(nodestore, node_id).await;
                    let copy = load_leaf_node(nodestore, copy_id).await;
                    assert_eq!(len, node.num_bytes() as usize);
                    assert_eq!(data_fixture(len, 1).as_ref(), node.data());
                    assert_eq!(len, copy.num_bytes() as usize);
                    assert_eq!(data_fixture(len, 1).as_ref(), copy.data());
                })
            })
            .await
        }

        #[tokio::test]
        async fn inner_node_one_child() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let leaf = new_full_leaf_node(nodestore).await.upcast();
                    let node = nodestore
                        .create_new_inner_node(1, &[*leaf.block_id()])
                        .await
                        .unwrap()
                        .upcast();
                    let copy = nodestore.create_new_node_as_copy_from(&node).await.unwrap();
                    assert_ne!(node.block_id(), copy.block_id());
                    assert_eq!(node.raw_blockdata(), copy.raw_blockdata());
                    let node_id = *node.block_id();
                    let copy_id = *copy.block_id();

                    //And data is correct after loading
                    drop(node);
                    drop(copy);
                    let node = load_inner_node(nodestore, node_id).await;
                    let copy = load_inner_node(nodestore, copy_id).await;
                    assert_eq!(1, node.depth().get());
                    assert_eq!(1, node.num_children().get());
                    assert_eq!(vec![*leaf.block_id()], node.children().collect::<Vec<_>>());
                    assert_eq!(1, copy.depth().get());
                    assert_eq!(1, copy.num_children().get());
                    assert_eq!(vec![*leaf.block_id()], copy.children().collect::<Vec<_>>());
                })
            })
            .await
        }

        #[tokio::test]
        async fn inner_node_two_children() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let leaf1 = new_full_leaf_node(nodestore).await.upcast();
                    let leaf2 = new_full_leaf_node(nodestore).await.upcast();
                    let node = nodestore
                        .create_new_inner_node(1, &[*leaf1.block_id(), *leaf2.block_id()])
                        .await
                        .unwrap()
                        .upcast();
                    let copy = nodestore.create_new_node_as_copy_from(&node).await.unwrap();
                    assert_ne!(node.block_id(), copy.block_id());
                    assert_eq!(node.raw_blockdata(), copy.raw_blockdata());
                    let node_id = *node.block_id();
                    let copy_id = *copy.block_id();

                    //And data is correct after loading
                    drop(node);
                    drop(copy);
                    let node = load_inner_node(nodestore, node_id).await;
                    let copy = load_inner_node(nodestore, copy_id).await;
                    assert_eq!(1, node.depth().get());
                    assert_eq!(2, node.num_children().get());
                    assert_eq!(
                        vec![*leaf1.block_id(), *leaf2.block_id()],
                        node.children().collect::<Vec<_>>()
                    );
                    assert_eq!(1, copy.depth().get());
                    assert_eq!(2, copy.num_children().get());
                    assert_eq!(
                        vec![*leaf1.block_id(), *leaf2.block_id()],
                        copy.children().collect::<Vec<_>>()
                    );
                })
            })
            .await
        }
    }

    mod num_nodes {
        use super::*;

        #[tokio::test]
        async fn empty() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    assert_eq!(0, nodestore.num_nodes().await.unwrap());
                })
            })
            .await
        }

        #[tokio::test]
        async fn after_adding_leaves() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    assert_eq!(0, nodestore.num_nodes().await.unwrap());
                    new_full_leaf_node(nodestore).await;
                    assert_eq!(1, nodestore.num_nodes().await.unwrap());
                    new_full_leaf_node(nodestore).await;
                    assert_eq!(2, nodestore.num_nodes().await.unwrap());
                })
            })
            .await
        }

        #[tokio::test]
        async fn after_removing_leaves() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let leaf1 = new_full_leaf_node(nodestore).await;
                    let leaf2 = new_full_leaf_node(nodestore).await;
                    assert_eq!(2, nodestore.num_nodes().await.unwrap());
                    leaf1.upcast().remove(nodestore).await.unwrap();
                    assert_eq!(1, nodestore.num_nodes().await.unwrap());
                    leaf2.upcast().remove(nodestore).await.unwrap();
                    assert_eq!(0, nodestore.num_nodes().await.unwrap());
                })
            })
            .await
        }

        #[tokio::test]
        async fn after_adding_leaves_and_inner_nodes() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    assert_eq!(0, nodestore.num_nodes().await.unwrap());
                    let leaf_id = *new_full_leaf_node(nodestore).await.block_id();
                    assert_eq!(1, nodestore.num_nodes().await.unwrap());
                    nodestore
                        .create_new_inner_node(1, &[leaf_id])
                        .await
                        .unwrap();
                    assert_eq!(2, nodestore.num_nodes().await.unwrap());
                })
            })
            .await
        }

        #[tokio::test]
        async fn after_removing_leaves_and_inner_nodes() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    assert_eq!(0, nodestore.num_nodes().await.unwrap());
                    let leaf = new_full_leaf_node(nodestore).await;
                    assert_eq!(1, nodestore.num_nodes().await.unwrap());
                    let inner = nodestore
                        .create_new_inner_node(1, &[*leaf.block_id()])
                        .await
                        .unwrap();
                    assert_eq!(2, nodestore.num_nodes().await.unwrap());

                    inner.upcast().remove(nodestore).await.unwrap();
                    assert_eq!(1, nodestore.num_nodes().await.unwrap());
                    leaf.upcast().remove(nodestore).await.unwrap();
                    assert_eq!(0, nodestore.num_nodes().await.unwrap());
                })
            })
            .await
        }
    }

    #[allow(non_snake_case)]
    mod remove_by_id {
        use super::*;

        #[tokio::test]
        async fn givenOtherwiseEmptyNodeStore_whenRemovingExistingLeaf_thenCannotBeLoadedAnymore() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let node_id = *new_full_leaf_node(nodestore).await.block_id();
                    assert!(nodestore.load(node_id).await.unwrap().is_some());

                    assert_eq!(
                        RemoveResult::SuccessfullyRemoved,
                        nodestore.remove_by_id(&node_id).await.unwrap()
                    );
                    assert!(nodestore.load(node_id).await.unwrap().is_none());
                })
            })
            .await
        }

        #[tokio::test]
        async fn givenOtherwiseEmptyNodeStore_whenRemovingExistingInnerNode_thenCannotBeLoadedAnymore(
        ) {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let leaf_id = *new_full_leaf_node(nodestore).await.block_id();
                    let node_id = *nodestore
                        .create_new_inner_node(1, &[leaf_id])
                        .await
                        .unwrap()
                        .block_id();
                    assert!(nodestore.load(node_id).await.unwrap().is_some());

                    assert_eq!(
                        RemoveResult::SuccessfullyRemoved,
                        nodestore.remove_by_id(&leaf_id).await.unwrap(),
                    );
                    assert_eq!(
                        RemoveResult::SuccessfullyRemoved,
                        nodestore.remove_by_id(&node_id).await.unwrap()
                    );
                    assert!(nodestore.load(node_id).await.unwrap().is_none());
                })
            })
            .await
        }

        #[tokio::test]
        async fn givenEmptyNodeStore_whenRemovingNonExistingEntry_thenFails() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    assert_eq!(
                        RemoveResult::NotRemovedBecauseItDoesntExist,
                        nodestore
                            .remove_by_id(
                                &BlockId::from_hex("3674b8dc1c3c1c41e331a1ebd4949087").unwrap()
                            )
                            .await
                            .unwrap()
                    );
                })
            })
            .await
        }

        #[tokio::test]
        async fn givenNodeStoreWithOtherEntries_whenRemovingExistingLeafNode_thenCannotBeLoadedAnymore(
        ) {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    new_full_inner_node(nodestore).await;

                    let node_id = *new_full_leaf_node(nodestore).await.block_id();
                    assert!(nodestore.load(node_id).await.unwrap().is_some());

                    assert_eq!(
                        RemoveResult::SuccessfullyRemoved,
                        nodestore.remove_by_id(&node_id).await.unwrap()
                    );
                    assert!(nodestore.load(node_id).await.unwrap().is_none());
                })
            })
            .await
        }

        #[tokio::test]
        async fn givenNodeStoreWithOtherEntries_whenRemovingExistingLeafNode_thenDoesntDeleteOtherNodes(
        ) {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let full_inner = *new_full_inner_node(nodestore).await.block_id();

                    let node_id = *new_full_leaf_node(nodestore).await.block_id();
                    assert!(nodestore.load(node_id).await.unwrap().is_some());

                    assert_eq!(
                        RemoveResult::SuccessfullyRemoved,
                        nodestore.remove_by_id(&node_id).await.unwrap()
                    );

                    assert_full_inner_node_is_valid(nodestore, full_inner).await;
                })
            })
            .await
        }

        #[tokio::test]
        async fn givenNodeStoreWithOtherEntries_whenRemovingExistingInnerNode_thenCannotBeLoadedAnymore(
        ) {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    new_full_inner_node(nodestore).await;

                    let leaf_id = *new_full_leaf_node(nodestore).await.block_id();
                    let node_id = *nodestore
                        .create_new_inner_node(1, &[leaf_id])
                        .await
                        .unwrap()
                        .block_id();
                    assert!(nodestore.load(node_id).await.unwrap().is_some());

                    assert_eq!(
                        RemoveResult::SuccessfullyRemoved,
                        nodestore.remove_by_id(&leaf_id).await.unwrap(),
                    );
                    assert_eq!(
                        RemoveResult::SuccessfullyRemoved,
                        nodestore.remove_by_id(&node_id).await.unwrap()
                    );
                    assert!(nodestore.load(node_id).await.unwrap().is_none());
                })
            })
            .await
        }

        #[tokio::test]
        async fn givenNodeStoreWithOtherEntries_whenRemovingExistingInnerNode_thenDoesntDeleteOtherEntries(
        ) {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let full_inner = *new_full_inner_node(nodestore).await.block_id();

                    let leaf_id = *new_full_leaf_node(nodestore).await.block_id();
                    let node_id = *nodestore
                        .create_new_inner_node(1, &[leaf_id])
                        .await
                        .unwrap()
                        .block_id();
                    assert!(nodestore.load(node_id).await.unwrap().is_some());

                    assert_eq!(
                        RemoveResult::SuccessfullyRemoved,
                        nodestore.remove_by_id(&leaf_id).await.unwrap(),
                    );
                    assert_eq!(
                        RemoveResult::SuccessfullyRemoved,
                        nodestore.remove_by_id(&node_id).await.unwrap()
                    );

                    assert_full_inner_node_is_valid(nodestore, full_inner).await;
                })
            })
            .await
        }

        #[tokio::test]
        async fn givenNodeStoreWithOtherEntries_whenRemovingNonExistingEntry_thenFails() {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    new_full_inner_node(nodestore).await;

                    assert_eq!(
                        RemoveResult::NotRemovedBecauseItDoesntExist,
                        nodestore
                            .remove_by_id(
                                &BlockId::from_hex("3674b8dc1c3c1c41e331a1ebd4949087").unwrap()
                            )
                            .await
                            .unwrap()
                    );
                })
            })
            .await
        }

        #[tokio::test]
        async fn givenNodeStoreWithOtherEntries_whenRemovingNonExistingEntry_thenDoesntDeleteOtherEntries(
        ) {
            with_nodestore(move |nodestore| {
                Box::pin(async move {
                    let full_inner = *new_full_inner_node(nodestore).await.block_id();

                    assert_eq!(
                        RemoveResult::NotRemovedBecauseItDoesntExist,
                        nodestore
                            .remove_by_id(
                                &BlockId::from_hex("3674b8dc1c3c1c41e331a1ebd4949087").unwrap()
                            )
                            .await
                            .unwrap()
                    );

                    assert_full_inner_node_is_valid(nodestore, full_inner).await;
                })
            })
            .await
        }
    }

    // TODO Test
    //  - new
    //  - layout
    //  - load
    //  - try_create_new_leaf_node
    //  - overwrite_leaf_node
    //  - estimate_space_for_num_blocks_left
    //  - virtual_block_size_bytes(&self)
    //  - flush_node
    //  - all_nodes
}
