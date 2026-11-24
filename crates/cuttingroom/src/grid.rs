use std::{
    cell::RefCell,
    collections::BTreeSet,
    io,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use async_channel::{Receiver, Sender, bounded};
use cap_std::fs::Dir;
use cr_project::PhotographGroup;
use slint::{
    ComponentHandle, Image, Model, ModelRc, Timer, VecModel, language::StandardListViewItem,
};

use crate::{
    ActiveSummary, FullPreview, GridPhoto, GroupDetails, MainWindow, MetadataEntry, MetadataFile,
    PhotoMetadata, PreviewState,
    image_decode::{DecodeError, Decoder, PreviewSize},
    metadata::{Fields, GroupMetadata, MetadataSource, read_group},
    preview::{self, Preview},
};

const MAX_VISIBLE_PREVIEWS: usize = 192;
const CACHE_MARGIN: usize = 12;
const UPDATE_INTERVAL: Duration = Duration::from_millis(16);

pub(crate) struct GridSession {
    pub(crate) grid: Rc<RefCell<Grid>>,
    completed: Receiver<(u64, usize, JobResult)>,
    task: slint::JoinHandle<()>,
    workers: [thread::JoinHandle<()>; 2],
}

pub(crate) struct Grid {
    directory: Option<Arc<Dir>>,
    groups: Vec<PhotographGroup>,
    photos: Rc<VecModel<GridPhoto>>,
    visible: Range<usize>,
    cached: BTreeSet<usize>,
    preview: JobQueue,
    metadata: JobQueue,
}

struct JobQueue {
    generation: Arc<AtomicU64>,
    in_flight: bool,
    jobs: Sender<Job>,
}

struct Job {
    directory: Arc<Dir>,
    paths: Vec<PathBuf>,
    index: usize,
    generation: u64,
    kind: JobKind,
}

#[derive(Clone, Copy)]
enum JobKind {
    Preview(PreviewSize),
    Summary,
    Details,
}

enum JobResult {
    Preview(PreviewSize, Preview),
    Summary(Option<MetadataSource>),
    Details(GroupMetadata),
}

impl JobQueue {
    fn start(
        name: &str,
        results: Sender<(u64, usize, JobResult)>,
    ) -> io::Result<(Self, thread::JoinHandle<()>)> {
        let (jobs, requests) = bounded::<Job>(1);
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        let worker = thread::Builder::new().name(name.into()).spawn(move || {
            let decoder = Decoder::new();
            while let Ok(job) = requests.recv_blocking() {
                let cancelled =
                    || requests.is_closed() || current.load(Ordering::Relaxed) != job.generation;
                let result = match job.kind {
                    JobKind::Preview(size) => JobResult::Preview(
                        size,
                        preview::load(&decoder, &job.directory, &job.paths, size, cancelled),
                    ),
                    JobKind::Summary => JobResult::Summary(preview::read_summary(
                        &decoder,
                        &job.directory,
                        &job.paths,
                        cancelled,
                    )),
                    JobKind::Details => JobResult::Details(read_group(
                        &decoder,
                        &job.directory,
                        &job.paths,
                        cancelled,
                    )),
                };
                if results
                    .send_blocking((job.generation, job.index, result))
                    .is_err()
                {
                    break;
                }
            }
        })?;
        Ok((
            Self {
                generation,
                in_flight: false,
                jobs,
            },
            worker,
        ))
    }
}

impl GridSession {
    pub(crate) fn start(window: &MainWindow) -> Result<Self, Box<dyn std::error::Error>> {
        let (results, completed) = bounded(2);
        let (preview, preview_worker) = JobQueue::start("preview worker", results.clone())?;
        let (metadata, metadata_worker) = JobQueue::start("metadata worker", results)?;
        let photos = Rc::new(VecModel::default());
        window.set_grid_photos(photos.clone().into());
        let grid = Rc::new(RefCell::new(Grid {
            directory: None,
            groups: Vec::new(),
            photos,
            visible: 0..0,
            cached: BTreeSet::new(),
            preview,
            metadata,
        }));

        let state = grid.clone();
        let weak = window.as_weak();
        window.on_grid_viewport_changed(move |first, end| {
            if let Some(window) = weak.upgrade() {
                let mut grid = state.borrow_mut();
                let first = first.max(0) as usize;
                let end = (end.max(0) as usize).min(first.saturating_add(MAX_VISIBLE_PREVIEWS));
                grid.visible = first..end;
                grid.refresh(&window);
            }
        });

        let state = grid.clone();
        let weak = window.as_weak();
        window.on_active_photo_changed(move || {
            if let Some(window) = weak.upgrade() {
                state.borrow_mut().refresh(&window);
            }
        });

        let state = grid.clone();
        let weak = window.as_weak();
        let updates = completed.clone();
        let task = slint::spawn_local(async move {
            while let Ok((generation, index, result)) = updates.recv().await {
                let Some(window) = weak.upgrade() else { break };
                let mut grid = state.borrow_mut();
                let is_preview = matches!(result, JobResult::Preview(..));
                grid.apply(&window, generation, index, result);
                if !is_preview {
                    grid.metadata.in_flight = false;
                    grid.request_next(&window);
                    continue;
                }

                let state = state.clone();
                let weak = weak.clone();
                Timer::single_shot(UPDATE_INTERVAL, move || {
                    if let Some(window) = weak.upgrade() {
                        let mut grid = state.borrow_mut();
                        grid.preview.in_flight = false;
                        grid.request_next(&window);
                    }
                });
            }
        })?;

        Ok(Self {
            grid,
            completed,
            task,
            workers: [preview_worker, metadata_worker],
        })
    }

    pub(crate) fn finish(self) -> io::Result<()> {
        self.grid.borrow().preview.jobs.close();
        self.grid.borrow().metadata.jobs.close();
        self.completed.close();
        self.task.abort();
        let [preview, metadata] = self.workers;
        let preview = preview.join();
        let metadata = metadata.join();
        preview
            .and(metadata)
            .map_err(|_| io::Error::other("photo worker failed"))
    }
}

impl Grid {
    pub(crate) fn replace(
        &mut self,
        window: &MainWindow,
        directory: Option<Dir>,
        groups: Vec<PhotographGroup>,
    ) {
        let active = active_index(window)
            .and_then(|index| self.groups.get(index))
            .and_then(|group| {
                groups
                    .iter()
                    .position(|candidate| candidate.stem == group.stem)
            });

        let photo_rows: Vec<_> = groups
            .iter()
            .map(|group| grid_photo(group, directory.is_some()))
            .collect();
        let file_rows: Vec<_> = photo_rows
            .iter()
            .map(|photo| StandardListViewItem::from(photo.relative_path.as_str()))
            .collect();
        self.directory = directory.map(Arc::new);
        self.groups = groups;
        self.preview.generation.fetch_add(1, Ordering::Relaxed);
        self.metadata.generation.fetch_add(1, Ordering::Relaxed);
        self.cached.clear();
        self.photos.set_vec(photo_rows);
        window.set_photo_files(Rc::new(VecModel::from(file_rows)).into());
        window.set_active_index(
            active.map_or(if self.groups.is_empty() { -1 } else { 0 }, |index| {
                index as i32
            }),
        );
        self.reset_preview(window, self.preview_target(window));
        self.reset_metadata(window);
        self.request_next(window);
    }

    fn refresh(&mut self, window: &MainWindow) {
        let target = self.preview_target(window);
        let preview_changed = target != window.get_full_preview().index;
        if preview_changed {
            self.preview.generation.fetch_add(1, Ordering::Relaxed);
            self.reset_preview(window, target);
        }
        if window.get_active_index() != window.get_active_summary().index {
            self.metadata.generation.fetch_add(1, Ordering::Relaxed);
            self.reset_metadata(window);
        }

        let retained = self.retained_range();
        let active = active_index(window);
        self.cached.retain(|index| {
            if retained.contains(index) || active == Some(*index) {
                return true;
            }
            if let Some(mut photo) = self.photos.row_data(*index) {
                photo.preview = Image::default();
                photo.state = PreviewState::Pending;
                photo.metadata = Default::default();
                self.photos.set_row_data(*index, photo);
            }
            false
        });
        self.request_next(window);
    }

    fn preview_target(&self, window: &MainWindow) -> i32 {
        if window.get_photo_view_active()
            && active_index(window).is_some_and(|index| index < self.groups.len())
        {
            window.get_active_index()
        } else {
            -1
        }
    }

    fn reset_preview(&self, window: &MainWindow, index: i32) {
        window.set_full_preview(FullPreview {
            index,
            state: if self.directory.is_some() {
                PreviewState::Pending
            } else {
                PreviewState::Unavailable
            },
            ..FullPreview::default()
        });
    }

    fn reset_metadata(&self, window: &MainWindow) {
        let index = window.get_active_index();
        let state = if self.directory.is_some() {
            PreviewState::Pending
        } else {
            PreviewState::Unavailable
        };
        window.set_active_summary(ActiveSummary {
            index,
            state,
            ..ActiveSummary::default()
        });
        window.set_group_details(GroupDetails {
            index,
            state,
            ..GroupDetails::default()
        });
    }

    fn request_next(&mut self, window: &MainWindow) {
        let Some(directory) = &self.directory else {
            return;
        };
        let metadata = self.next_metadata_job(window);
        let preview = self.next_preview_job(window);
        for (queue, next) in [(&mut self.metadata, metadata), (&mut self.preview, preview)] {
            if !queue.in_flight
                && let Some((index, kind)) = next
                && let Some(group) = self.groups.get(index)
            {
                queue.in_flight = queue
                    .jobs
                    .try_send(Job {
                        directory: directory.clone(),
                        paths: group.files.clone(),
                        index,
                        generation: queue.generation.load(Ordering::Relaxed),
                        kind,
                    })
                    .is_ok();
            }
        }
    }

    fn next_preview_job(&self, window: &MainWindow) -> Option<(usize, JobKind)> {
        if window.get_photo_view_active() {
            let preview = window.get_full_preview();
            if preview.state == PreviewState::Pending
                && let Ok(index) = preview.index.try_into()
            {
                return Some((index, JobKind::Preview(PreviewSize::Photo)));
            }
            return None;
        }
        let active = active_index(window);
        let next = |index: &usize| !self.cached.contains(index) && *index < self.groups.len();
        active
            .filter(|index| self.visible.contains(index) && next(index))
            .or_else(|| self.visible.clone().find(next))
            .or_else(|| active.filter(next))
            .map(|index| (index, JobKind::Preview(PreviewSize::Thumbnail)))
    }

    fn next_metadata_job(&self, window: &MainWindow) -> Option<(usize, JobKind)> {
        let index = active_index(window)?;
        if window.get_metadata_summary_visible()
            && window.get_active_summary().state == PreviewState::Pending
        {
            let preview = window.get_full_preview();
            let thumbnail_ready = self
                .photos
                .row_data(index)
                .is_some_and(|photo| photo.state == PreviewState::Ready);
            let photo_ready = preview.index == index as i32 && preview.state == PreviewState::Ready;
            if !(thumbnail_ready || photo_ready) {
                return Some((index, JobKind::Summary));
            }
        }
        if window.get_metadata_details_visible()
            && window.get_group_details().state == PreviewState::Pending
        {
            return Some((index, JobKind::Details));
        }
        None
    }

    fn apply(&mut self, window: &MainWindow, generation: u64, index: usize, result: JobResult) {
        let queue = if matches!(result, JobResult::Preview(..)) {
            &self.preview
        } else {
            &self.metadata
        };
        if generation != queue.generation.load(Ordering::Relaxed) {
            return;
        }
        match result {
            JobResult::Preview(size, preview) => self.apply_preview(window, index, size, preview),
            JobResult::Summary(summary) if active_index(window) == Some(index) => {
                let mut active = ActiveSummary {
                    index: index as i32,
                    state: PreviewState::Unavailable,
                    ..ActiveSummary::default()
                };
                if let Some(summary) = summary {
                    active.state = PreviewState::Ready;
                    active.metadata = PhotoMetadata {
                        filename: summary.filename.into(),
                        summary: metadata_model(summary.fields),
                    };
                }
                window.set_active_summary(active);
            }
            JobResult::Details(details) if active_index(window) == Some(index) => {
                window.set_group_details(GroupDetails {
                    index: index as i32,
                    state: PreviewState::Ready,
                    files: metadata_files(details.files),
                    sidecars: metadata_files(details.sidecars),
                });
            }
            JobResult::Summary(_) | JobResult::Details(_) => (),
        }
    }

    fn apply_preview(
        &mut self,
        window: &MainWindow,
        index: usize,
        size: PreviewSize,
        preview: Preview,
    ) {
        let metadata = PhotoMetadata {
            filename: preview.filename.into(),
            summary: metadata_model(preview.summary),
        };
        if let PreviewSize::Photo = size {
            if window.get_photo_view_active() && active_index(window) == Some(index) {
                let mut full_preview = FullPreview {
                    index: index as i32,
                    metadata,
                    ..FullPreview::default()
                };
                full_preview.state = match preview.pixels {
                    Ok(pixels) => {
                        full_preview.image = Image::from_rgba8(pixels);
                        PreviewState::Ready
                    }
                    Err(error) => preview_error_state(error),
                };
                window.set_full_preview(full_preview);
            }
            return;
        }
        if !(self.retained_range().contains(&index) || active_index(window) == Some(index)) {
            return;
        }
        if let Some(mut photo) = self.photos.row_data(index) {
            photo.metadata = metadata;
            photo.state = match preview.pixels {
                Ok(pixels) => {
                    photo.preview = Image::from_rgba8(pixels);
                    PreviewState::Ready
                }
                Err(error) => preview_error_state(error),
            };
            self.cached.insert(index);
            self.photos.set_row_data(index, photo);
        }
    }

    fn retained_range(&self) -> Range<usize> {
        self.visible.start.saturating_sub(CACHE_MARGIN)
            ..self.visible.end.saturating_add(CACHE_MARGIN)
    }
}

fn metadata_files(files: Vec<MetadataSource>) -> ModelRc<MetadataFile> {
    Rc::new(VecModel::from_iter(files.into_iter().map(|file| {
        MetadataFile {
            filename: file.filename.into(),
            fields: metadata_model(file.fields),
        }
    })))
    .into()
}

fn metadata_model(fields: Fields) -> ModelRc<MetadataEntry> {
    Rc::new(VecModel::from_iter(fields.into_iter().map(
        |(label, value)| MetadataEntry {
            label: label.into(),
            value: value.into(),
        },
    )))
    .into()
}

fn preview_error_state(error: DecodeError) -> PreviewState {
    match error {
        DecodeError::Unavailable => PreviewState::Unavailable,
        DecodeError::DecoderUnavailable => PreviewState::DecoderUnavailable,
    }
}

fn active_index(window: &MainWindow) -> Option<usize> {
    window.get_active_index().try_into().ok()
}

fn grid_photo(group: &PhotographGroup, directory_open: bool) -> GridPhoto {
    let path = match group.files.as_slice() {
        [path] => path,
        _ => &group.stem,
    };
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let formats: BTreeSet<_> = group.files.iter().map(|path| format_label(path)).collect();
    GridPhoto {
        name: name.as_ref().into(),
        relative_path: path.to_string_lossy().as_ref().into(),
        formats: Rc::new(VecModel::from_iter(formats)).into(),
        state: if directory_open {
            PreviewState::Pending
        } else {
            PreviewState::Unavailable
        },
        ..GridPhoto::default()
    }
}

fn format_label(path: &Path) -> slint::SharedString {
    let extension = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_uppercase();
    match extension.as_str() {
        "JPG" => "JPEG",
        "TIF" => "TIFF",
        format => format,
    }
    .into()
}
