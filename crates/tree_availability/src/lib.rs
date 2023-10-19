pub mod abi;
pub mod block_scanner;
pub mod error;
pub mod server;
pub mod tree;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Json, Router};
use error::TreeAvailabilityError;
use ethers::contract::EthEvent;
use ethers::providers::{Middleware, StreamExt};
use ethers::types::{Filter, Log, H160};
use semaphore::lazy_merkle_tree::Canonical;
use tokio::task::JoinHandle;
use tree::{Hash, PoseidonTree, WorldTree};

use crate::abi::TreeChangedFilter;
use crate::server::inclusion_proof;

// TODO: Change to a configurable parameter and also set a default
const TREE_HISTORY_SIZE: usize = 1000;
const DEFAULT_PORT: u16 = 8080;
const DEFAULT_SOCKET_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), DEFAULT_PORT);

pub struct TreeAvailabilityService<M: Middleware + 'static> {
    pub world_tree: Arc<WorldTree<M>>,
}

impl<M: Middleware> TreeAvailabilityService<M> {
    pub fn new(
        tree_depth: usize,
        dense_prefix_depth: usize,
        world_tree_address: H160,
        world_tree_creation_block: u64,
        middleware: Arc<M>,
    ) -> Self {
        let tree = PoseidonTree::<Canonical>::new_with_dense_prefix(
            tree_depth,
            dense_prefix_depth,
            &Hash::ZERO,
        );

        let world_tree = Arc::new(WorldTree::new(
            tree,
            TREE_HISTORY_SIZE,
            world_tree_address,
            world_tree_creation_block,
            middleware,
        ));

        Self { world_tree }
    }

    pub async fn spawn(
        &self,
    ) -> Vec<JoinHandle<Result<(), TreeAvailabilityError<M>>>> {
        let mut handles = vec![];

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Log>(100);
        let tx_middleware = self.world_tree.middleware.clone();
        let rx_middleware = tx_middleware.clone();

        let filter = Filter::new()
            .address(self.world_tree.address)
            .topic0(TreeChangedFilter::signature());

        // Spawn a thread to listen to tree changed events with a buffer
        handles.push(tokio::spawn(async move {
            let mut stream = tx_middleware
                .watch(&filter)
                .await
                .expect("TODO: Handle/Propagate this error")
                .stream();

            while let Some(log) = stream.next().await {
                tx.send(log)
                    .await
                    .expect("TODO: Handle/Propagate this error");
            }

            Ok(())
        }));

        // Sync the world tree to the chain head
        self.world_tree
            .sync_to_head()
            .await
            .expect("TODO: error handling");

        let world_tree = self.world_tree.clone();

        handles.push(tokio::spawn(async move {
            while let Some(log) = rx.recv().await {
                world_tree.sync_from_log(log).await?;
            }

            Ok(())
        }));

        handles
    }

    pub async fn serve(
        self,
        address: Option<SocketAddr>,
    ) -> Vec<JoinHandle<Result<(), TreeAvailabilityError<M>>>> {
        let mut handles = vec![];

        // Spawn a new task to keep the world tree synced to the chain head
        let world_tree_handle = self.spawn().await;
        handles.push(world_tree_handle);

        // Initialize a new router and spawn the server
        let router = axum::Router::new()
            .route("/inclusionProof", axum::routing::post(inclusion_proof))
            .with_state(self.world_tree.clone());

        let address = address.unwrap_or_else(|| DEFAULT_SOCKET_ADDR);

        let server_handle = tokio::spawn(async move {
            axum::Server::bind(&address)
                .serve(router.into_make_service())
                .await
                .map_err(TreeAvailabilityError::HyperError)?;
            // .with_graceful_shutdown(await_shutdown());

            Ok(())
        });

        handles.push(server_handle);

        handles
    }
}

//TODO: extend api and it returns endpoints and functions that are called from the endpoint? That way you can add the api extension to your api

//TODO: implement the api trait

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use ethers::providers::{Provider, Ws};
    use ethers::types::H160;

    use crate::TreeAvailabilityService;

    //TODO: set world tree address as const for tests

    async fn test_spawn_tree_availability_service() -> eyre::Result<()> {
        let world_tree_address =
            H160::from_str("0x78eC127A3716D447F4575E9c834d452E397EE9E1")?;

        let middleware = Arc::new(
            Provider::<Ws>::connect(std::env::var("GOERLI_WS_ENDPOINT")?)
                .await?,
        );

        let tree_availability_service = TreeAvailabilityService::new(
            30,
            10,
            world_tree_address,
            0,
            middleware,
        );

        let _handle = tree_availability_service.spawn().await;

        Ok(())
    }
}
