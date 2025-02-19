use crate::{
	job::{JobError, JobReportUpdate, JobResult, JobState, StatefulJob, WorkerContext},
	library::Library,
	location::file_path_helper::{
		ensure_sub_path_is_directory, ensure_sub_path_is_in_location,
		file_path_just_materialized_path_cas_id, MaterializedPath,
	},
	prisma::{file_path, location, PrismaClient},
};

use std::{collections::VecDeque, hash::Hash, path::PathBuf};

use sd_file_ext::extensions::Extension;

use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::info;

use super::{
	finalize_thumbnailer, process_step, ThumbnailerError, ThumbnailerJobReport,
	ThumbnailerJobState, ThumbnailerJobStep, ThumbnailerJobStepKind, FILTERED_IMAGE_EXTENSIONS,
	THUMBNAIL_CACHE_DIR_NAME,
};

#[cfg(feature = "ffmpeg")]
use super::FILTERED_VIDEO_EXTENSIONS;

pub const THUMBNAILER_JOB_NAME: &str = "thumbnailer";

pub struct ThumbnailerJob {}

#[derive(Serialize, Deserialize, Clone)]
pub struct ThumbnailerJobInit {
	pub location: location::Data,
	pub sub_path: Option<PathBuf>,
	pub background: bool,
}

impl Hash for ThumbnailerJobInit {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.location.id.hash(state);
		if let Some(ref sub_path) = self.sub_path {
			sub_path.hash(state);
		}
	}
}

#[async_trait::async_trait]
impl StatefulJob for ThumbnailerJob {
	type Init = ThumbnailerJobInit;
	type Data = ThumbnailerJobState;
	type Step = ThumbnailerJobStep;

	fn name(&self) -> &'static str {
		THUMBNAILER_JOB_NAME
	}

	async fn init(&self, ctx: WorkerContext, state: &mut JobState<Self>) -> Result<(), JobError> {
		let Library { db, .. } = &ctx.library;

		let thumbnail_dir = ctx
			.library
			.config()
			.data_directory()
			.join(THUMBNAIL_CACHE_DIR_NAME);

		let location_id = state.init.location.id;
		let location_path = PathBuf::from(&state.init.location.path);

		let materialized_path = if let Some(ref sub_path) = state.init.sub_path {
			let full_path = ensure_sub_path_is_in_location(&location_path, sub_path)
				.await
				.map_err(ThumbnailerError::from)?;
			ensure_sub_path_is_directory(&location_path, sub_path)
				.await
				.map_err(ThumbnailerError::from)?;

			MaterializedPath::new(location_id, &location_path, &full_path, true)
				.map_err(ThumbnailerError::from)?
		} else {
			MaterializedPath::new(location_id, &location_path, &location_path, true)
				.map_err(ThumbnailerError::from)?
		};

		info!("Searching for images in location {location_id} at directory {materialized_path}");

		// create all necessary directories if they don't exist
		fs::create_dir_all(&thumbnail_dir).await?;

		// query database for all image files in this location that need thumbnails
		let image_files = get_files_by_extensions(
			db,
			&materialized_path,
			&FILTERED_IMAGE_EXTENSIONS,
			ThumbnailerJobStepKind::Image,
		)
		.await?;
		info!("Found {:?} image files", image_files.len());

		#[cfg(feature = "ffmpeg")]
		let all_files = {
			// query database for all video files in this location that need thumbnails
			let video_files = get_files_by_extensions(
				db,
				&materialized_path,
				&FILTERED_VIDEO_EXTENSIONS,
				ThumbnailerJobStepKind::Video,
			)
			.await?;
			info!("Found {:?} video files", video_files.len());

			image_files
				.into_iter()
				.chain(video_files.into_iter())
				.collect::<VecDeque<_>>()
		};
		#[cfg(not(feature = "ffmpeg"))]
		let all_files = { image_files.into_iter().collect::<VecDeque<_>>() };

		ctx.progress(vec![
			JobReportUpdate::TaskCount(all_files.len()),
			JobReportUpdate::Message(format!("Preparing to process {} files", all_files.len())),
		]);

		state.data = Some(ThumbnailerJobState {
			thumbnail_dir,
			location_path,
			report: ThumbnailerJobReport {
				location_id,
				materialized_path: materialized_path.into(),
				thumbnails_created: 0,
			},
		});
		state.steps = all_files;

		Ok(())
	}

	async fn execute_step(
		&self,
		ctx: WorkerContext,
		state: &mut JobState<Self>,
	) -> Result<(), JobError> {
		process_step(
			state.init.background,
			state.step_number,
			&state.steps[0],
			state
				.data
				.as_mut()
				.expect("critical error: missing data on job state"),
			ctx,
		)
		.await
	}

	async fn finalize(&mut self, ctx: WorkerContext, state: &mut JobState<Self>) -> JobResult {
		finalize_thumbnailer(
			state
				.data
				.as_ref()
				.expect("critical error: missing data on job state"),
			ctx,
		)
	}
}

async fn get_files_by_extensions(
	db: &PrismaClient,
	materialized_path: &MaterializedPath,
	extensions: &[Extension],
	kind: ThumbnailerJobStepKind,
) -> Result<Vec<ThumbnailerJobStep>, JobError> {
	Ok(db
		.file_path()
		.find_many(vec![
			file_path::location_id::equals(materialized_path.location_id()),
			file_path::extension::in_vec(extensions.iter().map(ToString::to_string).collect()),
			file_path::materialized_path::starts_with(materialized_path.into()),
		])
		.select(file_path_just_materialized_path_cas_id::select())
		.exec()
		.await?
		.into_iter()
		.map(|file_path| ThumbnailerJobStep { file_path, kind })
		.collect())
}
