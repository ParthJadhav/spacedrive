use std::{
	sync::Arc,
	time::{Duration, Instant},
};

use rspc::{Config, Type};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{
	job::JobManager,
	library::LibraryManager,
	node::{NodeConfig, NodeConfigManager},
	p2p::P2PManager,
	util::secure_temp_keystore::SecureTempKeystore,
};

use utils::{InvalidRequests, InvalidateOperationEvent};

pub type Router = rspc::Router<Ctx>;
pub(crate) type RouterBuilder = rspc::RouterBuilder<Ctx>;

/// Represents an internal core event, these are exposed to client via a rspc subscription.
#[derive(Debug, Clone, Serialize, Type)]
pub enum CoreEvent {
	NewThumbnail { cas_id: String },
	InvalidateOperation(InvalidateOperationEvent),
	InvalidateOperationDebounced(InvalidateOperationEvent),
}

/// Is provided when executing the router from the request.
pub struct Ctx {
	pub library_manager: Arc<LibraryManager>,
	pub config: Arc<NodeConfigManager>,
	pub jobs: Arc<JobManager>,
	pub event_bus: broadcast::Sender<CoreEvent>,
	pub p2p: Arc<P2PManager>,
	pub secure_temp_keystore: Arc<SecureTempKeystore>,
}

mod files;
mod jobs;
mod keys;
mod libraries;
mod locations;
mod nodes;
mod p2p;
mod tags;
pub mod utils;
pub mod volumes;

#[derive(Serialize, Deserialize, Debug, Type)]
struct NodeState {
	#[serde(flatten)]
	config: NodeConfig,
	data_path: String,
}

pub(crate) fn mount() -> Arc<Router> {
	let config = Config::new().set_ts_bindings_header("/* eslint-disable */");

	#[cfg(all(debug_assertions, not(feature = "mobile")))]
	let config = config.export_ts_bindings(
		std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../packages/client/src/core.ts"),
	);

	let r = <Router>::new()
		.config(config)
		.query("buildInfo", |t| {
			#[derive(Serialize, Type)]
			pub struct BuildInfo {
				version: &'static str,
				commit: &'static str,
			}

			t(|_, _: ()| BuildInfo {
				version: env!("CARGO_PKG_VERSION"),
				commit: env!("GIT_HASH"),
			})
		})
		.query("nodeState", |t| {
			t(|ctx, _: ()| async move {
				Ok(NodeState {
					config: ctx.config.get().await,
					// We are taking the assumption here that this value is only used on the frontend for display purposes
					data_path: ctx
						.config
						.data_directory()
						.to_str()
						.expect("Found non-UTF-8 path")
						.to_string(),
				})
			})
		})
		.yolo_merge("library.", libraries::mount())
		.yolo_merge("volumes.", volumes::mount())
		.yolo_merge("tags.", tags::mount())
		.yolo_merge("nodes.", nodes::mount())
		.yolo_merge("keys.", keys::mount())
		.yolo_merge("locations.", locations::mount())
		.yolo_merge("files.", files::mount())
		.yolo_merge("jobs.", jobs::mount())
		.yolo_merge("p2p.", p2p::mount())
		// TODO: Scope the invalidate queries to a specific library (filtered server side)
		.subscription("invalidateQuery", |t| {
			t(|ctx, _: ()| {
				let mut event_bus_rx = ctx.event_bus.subscribe();
				let mut last = Instant::now();
				async_stream::stream! {
					while let Ok(event) = event_bus_rx.recv().await {
						match event {
							CoreEvent::InvalidateOperation(op) => yield op,
							CoreEvent::InvalidateOperationDebounced(op) => {
								let current = Instant::now();
								if current.duration_since(last) > Duration::from_millis(1000 / 10) {
									last = current;
									yield op;
								}
							},
							_ => {}
						}
					}
				}
			})
		})
		.build()
		.arced();
	InvalidRequests::validate(r.clone()); // This validates all invalidation calls.

	r
}

#[cfg(test)]
mod tests {
	/// This test will ensure the rspc router and all calls to `invalidate_query` are valid and also export an updated version of the Typescript bindings.
	#[test]
	fn test_and_export_rspc_bindings() {
		super::mount();
	}
}
