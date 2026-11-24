use std::{
    cell::RefCell,
    collections::{BTreeSet, VecDeque},
    io,
    ops::Range,
    path::Path,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use async_channel::{Receiver, Sender, bounded};
use cap_std::fs::Dir;
use cr_project::PhotographGroup;
use slint::{ComponentHandle, Image, Model, ModelRc, VecModel, language::StandardListViewItem};

mod thumbnail_memory;

use self::thumbnail_memory::ThumbnailMemory;
use crate::{
    ActiveSummary, FullPreview, GridPhoto, GroupDetails, MainWindow, MetadataEntry, MetadataFile,
    PhotoMetadata, PreviewState, ThumbnailProgress,
    app::ProjectRequest,
    image_decode::{DecodeError, Decoder, PreviewSize},
    metadata::{Fields, GroupMetadata, MetadataSource, read_group},
    preview::{self, Preview},
    thumbnail_cache::{CacheStatus, LoadedThumbnail, ThumbnailCache, ThumbnailEntry},
};

const MAX_VISIBLE_PREVIEWS: usize = 192;
const CACHE_MARGIN: usize = 12;
const MAX_PENDING_WRITES: usize = 24;
const UPDATE_BATCH_SIZE: usize = 8;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) struct ThumbnailInventory {
    pub(crate) scan: u64,
    pub(crate) cache: Option<ThumbnailCache>,
    pub(crate) unlisted_total: usize,
    pub(crate) unlisted_unavailable: bool,
}

#[derive(Default)]
struct ThumbnailRun {
    scan: u64,
    cache: Option<Arc<ThumbnailCache>>,
    displayed: Vec<ThumbnailState>,
    next_displayed: usize,
    unlisted: VecDeque<Arc<PhotographGroup>>,
    unlisted_remaining: usize,
    waiting_for_batch: bool,
    total: usize,
    generated: usize,
    finished: usize,
    saved: usize,
    incomplete: bool,
    last_update: Option<(Instant, ThumbnailPhase, usize)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThumbnailPhase {
    Generating,
    Saving,
    Finished,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThumbnailState {
    Pending,
    Deferred,
    Writing,
    Saved,
    Unavailable,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RebuildState {
    Idle,
    WaitingForWorkers,
    Requested,
}

impl ThumbnailRun {
    fn new(inventory: ThumbnailInventory, displayed: usize) -> Self {
        let total = displayed + inventory.unlisted_total;
        let done = if inventory.cache.is_none() { total } else { 0 };
        Self {
            scan: inventory.scan,
            cache: inventory.cache.map(Arc::new),
            displayed: vec![ThumbnailState::Pending; displayed],
            unlisted_remaining: inventory.unlisted_total,
            incomplete: inventory.unlisted_unavailable,
            total,
            generated: done,
            finished: done,
            ..Self::default()
        }
    }

    fn next_displayed(&mut self) -> Option<usize> {
        while let Some(done) = self.displayed.get(self.next_displayed) {
            if matches!(done, ThumbnailState::Pending | ThumbnailState::Deferred) {
                return Some(self.next_displayed);
            }
            self.next_displayed += 1;
        }
        None
    }

    fn needs_thumbnail(&self, index: Option<usize>) -> bool {
        self.cache.is_some()
            && index.is_none_or(|index| {
                matches!(
                    self.displayed.get(index),
                    Some(ThumbnailState::Pending | ThumbnailState::Deferred)
                )
            })
    }

    fn record_load(&mut self, index: Option<usize>, state: ThumbnailState) {
        let previous = index
            .and_then(|index| self.displayed.get_mut(index))
            .map_or(ThumbnailState::Pending, |slot| {
                std::mem::replace(slot, state)
            });
        self.generated += usize::from(previous == ThumbnailState::Pending);
        if matches!(state, ThumbnailState::Saved | ThumbnailState::Unavailable) {
            self.finished += 1;
            self.saved += usize::from(state == ThumbnailState::Saved);
        }
    }

    fn record_save(&mut self, index: Option<usize>, saved: bool) {
        if let Some(slot) = index.and_then(|index| self.displayed.get_mut(index)) {
            *slot = if saved {
                ThumbnailState::Saved
            } else {
                ThumbnailState::Unavailable
            };
        }
        self.finished += 1;
        self.saved += usize::from(saved);
    }

    fn publish(&mut self, window: &MainWindow) {
        let phase = if self.finished == self.total {
            ThumbnailPhase::Finished
        } else if self.generated == self.total {
            ThumbnailPhase::Saving
        } else {
            ThumbnailPhase::Generating
        };
        let completed = if phase == ThumbnailPhase::Saving {
            self.finished
        } else {
            self.generated
        };
        if self.last_update.is_some_and(|(last, previous, count)| {
            phase == previous
                && (phase == ThumbnailPhase::Finished
                    || (last.elapsed() < PROGRESS_INTERVAL
                        && completed.saturating_sub(count) < MAX_PENDING_WRITES))
        }) {
            return;
        }
        let count = |value| i32::try_from(value).unwrap_or(i32::MAX);
        window.set_thumbnail_progress(ThumbnailProgress {
            total: count(self.total),
            generated: count(self.generated),
            finished: count(self.finished),
            cached: count(self.saved),
            incomplete: self.incomplete,
        });
        self.last_update = Some((Instant::now(), phase, completed));
    }
}

pub(crate) struct GridSession {
    pub(crate) grid: Rc<RefCell<Grid>>,
    completed: Receiver<(u64, Option<usize>, JobResult)>,
    task: slint::JoinHandle<()>,
    workers: [thread::JoinHandle<()>; 3],
}

pub(crate) struct Grid {
    directory: Option<Arc<Dir>>,
    groups: Vec<Arc<PhotographGroup>>,
    photos: Rc<VecModel<GridPhoto>>,
    visible: Range<usize>,
    cached: BTreeSet<usize>,
    preview: JobQueue,
    metadata: JobQueue,
    writer: ThumbnailWriter,
    thumbnails: ThumbnailRun,
    rebuild: RebuildState,
    project_requests: Sender<ProjectRequest>,
}

struct JobQueue {
    generation: Arc<AtomicU64>,
    in_flight: Option<JobKind>,
    jobs: Sender<Job>,
}

struct Job {
    directory: Arc<Dir>,
    group: Arc<PhotographGroup>,
    index: Option<usize>,
    generation: u64,
    kind: JobKind,
    thumbnail_cache: Option<Arc<ThumbnailCache>>,
}

#[derive(Clone, Copy)]
enum JobKind {
    Preview(PreviewSize),
    Thumbnail { scan: u64, visible: bool },
    Summary,
    Details,
}

enum JobResult {
    Preview(PreviewSize, Preview),
    Thumbnail(Preview, ThumbnailOutcome),
    Saved(bool),
    Summary(Option<MetadataSource>),
    Details(GroupMetadata),
}

enum ThumbnailOutcome {
    Hit,
    Write(WriteRequest),
    Unavailable,
}

struct WriteRequest {
    directory: Arc<Dir>,
    group: Arc<PhotographGroup>,
    cache: Arc<ThumbnailCache>,
    entry: ThumbnailEntry,
}

struct WriteJob {
    request: WriteRequest,
    preview: Preview,
    generation: u64,
    index: Option<usize>,
}

struct ThumbnailWriter {
    jobs: Sender<WriteJob>,
    generation: Arc<AtomicU64>,
    in_flight: usize,
}

impl ThumbnailWriter {
    fn start(
        results: Sender<(u64, Option<usize>, JobResult)>,
    ) -> io::Result<(Self, thread::JoinHandle<()>)> {
        let (jobs, requests) = bounded::<WriteJob>(MAX_PENDING_WRITES);
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        let worker = thread::Builder::new()
            .name("thumbnail writer".into())
            .spawn(move || {
                while let Ok(job) = requests.recv_blocking() {
                    let cancelled = || {
                        requests.is_closed() || current.load(Ordering::Relaxed) != job.generation
                    };
                    let request = job.request;
                    let saved = request.cache.save(
                        &request.directory,
                        &request.group,
                        request.entry,
                        &job.preview,
                        cancelled,
                    );
                    if results
                        .send_blocking((job.generation, job.index, JobResult::Saved(saved)))
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok((
            Self {
                jobs,
                generation,
                in_flight: 0,
            },
            worker,
        ))
    }

    fn submit(&mut self, request: WriteRequest, preview: &Preview, index: Option<usize>) -> bool {
        let queued = self.has_room()
            && self
                .jobs
                .try_send(WriteJob {
                    request,
                    preview: preview.clone(),
                    generation: self.generation.load(Ordering::Relaxed),
                    index,
                })
                .is_ok();
        self.in_flight += usize::from(queued);
        queued
    }

    fn has_room(&self) -> bool {
        self.in_flight < MAX_PENDING_WRITES
    }
}

impl JobQueue {
    fn start(
        name: &str,
        results: Sender<(u64, Option<usize>, JobResult)>,
    ) -> io::Result<(Self, thread::JoinHandle<()>)> {
        let (jobs, requests) = bounded::<Job>(1);
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        let worker = thread::Builder::new().name(name.into()).spawn(move || {
            let decoder = Decoder::new();
            let mut memory = ThumbnailMemory::default();
            while let Ok(job) = requests.recv_blocking() {
                let cancelled =
                    || requests.is_closed() || current.load(Ordering::Relaxed) != job.generation;
                let result = match job.kind {
                    JobKind::Thumbnail { scan, visible } => {
                        let generate = || {
                            preview::load(
                                &decoder,
                                &job.directory,
                                &job.group.files,
                                PreviewSize::Thumbnail,
                                cancelled,
                            )
                        };
                        let thumbnail = memory.load(scan, job.index, visible, || {
                            if let Some(cache) = &job.thumbnail_cache {
                                cache.load_or_generate(
                                    &job.directory,
                                    &job.group,
                                    generate,
                                    cancelled,
                                )
                            } else {
                                LoadedThumbnail {
                                    preview: generate(),
                                    status: CacheStatus::Unavailable,
                                }
                            }
                        });
                        let outcome = match (thumbnail.status, job.thumbnail_cache) {
                            (CacheStatus::Hit, _) => ThumbnailOutcome::Hit,
                            (CacheStatus::Write(entry), Some(cache)) => {
                                ThumbnailOutcome::Write(WriteRequest {
                                    directory: job.directory,
                                    group: job.group,
                                    cache,
                                    entry,
                                })
                            }
                            _ => ThumbnailOutcome::Unavailable,
                        };
                        JobResult::Thumbnail(thumbnail.preview, outcome)
                    }
                    JobKind::Preview(size) => JobResult::Preview(
                        size,
                        preview::load(&decoder, &job.directory, &job.group.files, size, cancelled),
                    ),
                    JobKind::Summary => JobResult::Summary(preview::read_summary(
                        &decoder,
                        &job.directory,
                        &job.group.files,
                        cancelled,
                    )),
                    JobKind::Details => JobResult::Details(read_group(
                        &decoder,
                        &job.directory,
                        &job.group.files,
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
                in_flight: None,
                jobs,
            },
            worker,
        ))
    }

    fn submit(
        &mut self,
        directory: &Arc<Dir>,
        group: &Arc<PhotographGroup>,
        index: Option<usize>,
        kind: JobKind,
        thumbnail_cache: Option<Arc<ThumbnailCache>>,
    ) -> bool {
        if self.in_flight.is_some() {
            return false;
        }
        let sent = self
            .jobs
            .try_send(Job {
                directory: directory.clone(),
                group: group.clone(),
                index,
                generation: self.generation.load(Ordering::Relaxed),
                kind,
                thumbnail_cache,
            })
            .is_ok();
        if sent {
            self.in_flight = Some(kind);
        }
        sent
    }
}

impl GridSession {
    pub(crate) fn start(
        window: &MainWindow,
        project_requests: Sender<ProjectRequest>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (results, completed) = bounded(3);
        let (preview, preview_worker) = JobQueue::start("preview worker", results.clone())?;
        let (metadata, metadata_worker) = JobQueue::start("metadata worker", results.clone())?;
        let (writer, writer_worker) = ThumbnailWriter::start(results)?;
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
            writer,
            thumbnails: ThumbnailRun::default(),
            rebuild: RebuildState::Idle,
            project_requests,
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
            let (resume, yielded) = bounded(1);
            let mut count = 0;
            while let Ok((generation, index, result)) = updates.recv().await {
                let Some(window) = weak.upgrade() else { break };
                {
                    let mut grid = state.borrow_mut();
                    grid.apply(&window, generation, index, result);
                    grid.request_next(&window);
                }
                count += 1;
                if count == UPDATE_BATCH_SIZE {
                    count = 0;
                    let resume = resume.clone();
                    if slint::invoke_from_event_loop(move || {
                        let _ = resume.try_send(());
                    })
                    .is_err()
                        || yielded.recv().await.is_err()
                    {
                        break;
                    }
                }
            }
        })?;

        Ok(Self {
            grid,
            completed,
            task,
            workers: [preview_worker, metadata_worker, writer_worker],
        })
    }

    pub(crate) fn finish(self) -> io::Result<()> {
        self.grid.borrow().preview.jobs.close();
        self.grid.borrow().metadata.jobs.close();
        self.grid.borrow().writer.jobs.close();
        self.completed.close();
        self.task.abort();
        let mut result = Ok(());
        for worker in self.workers {
            if worker.join().is_err() {
                result = Err(io::Error::other("photo worker failed"));
            }
        }
        result
    }
}

impl Grid {
    pub(crate) fn replace(
        &mut self,
        window: &MainWindow,
        directory: Option<Dir>,
        groups: Vec<PhotographGroup>,
        thumbnails: ThumbnailInventory,
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
        self.thumbnails = ThumbnailRun::new(thumbnails, groups.len());
        self.rebuild = RebuildState::Idle;
        self.groups = groups.into_iter().map(Arc::new).collect();
        self.preview.generation.fetch_add(1, Ordering::Relaxed);
        self.metadata.generation.fetch_add(1, Ordering::Relaxed);
        self.writer.generation.fetch_add(1, Ordering::Relaxed);
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
        self.thumbnails.publish(window);
        self.request_next(window);
    }

    pub(crate) fn accept_thumbnail_batch(
        &mut self,
        window: &MainWindow,
        scan: u64,
        groups: Vec<PhotographGroup>,
    ) {
        let thumbnails = &mut self.thumbnails;
        if scan != thumbnails.scan || !thumbnails.waiting_for_batch {
            return;
        }
        thumbnails.waiting_for_batch = false;
        if groups.is_empty() || groups.len() > thumbnails.unlisted_remaining {
            let missing = std::mem::take(&mut thumbnails.unlisted_remaining);
            thumbnails.generated += missing;
            thumbnails.finished += missing;
            thumbnails.publish(window);
        } else {
            thumbnails.unlisted_remaining -= groups.len();
            thumbnails.unlisted.extend(groups.into_iter().map(Arc::new));
        }
        self.request_next(window);
    }

    pub(crate) fn rebuild_thumbnail_cache(&mut self, window: &MainWindow) {
        self.rebuild = RebuildState::WaitingForWorkers;
        window.set_thumbnail_reset_active(true);
        window.set_status_text("Clearing thumbnail cache…".into());
        self.request_next(window);
    }

    pub(crate) fn resume_after_scan_failure(&mut self, window: &MainWindow) {
        self.rebuild = RebuildState::Idle;
        self.request_next(window);
    }

    fn refresh(&mut self, window: &MainWindow) {
        let target = self.preview_target(window);
        let preview_changed = target != window.get_full_preview().index;
        if preview_changed {
            if matches!(
                self.preview.in_flight,
                Some(JobKind::Preview(PreviewSize::Photo))
            ) {
                self.preview.generation.fetch_add(1, Ordering::Relaxed);
            }
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
        if self.rebuild == RebuildState::WaitingForWorkers
            && self.preview.in_flight.is_none()
            && self.writer.in_flight == 0
        {
            let sent = self.directory.as_ref().is_some_and(|directory| {
                self.project_requests
                    .try_send(ProjectRequest::RebuildThumbnailCache(
                        directory.clone(),
                        self.thumbnails.cache.clone(),
                    ))
                    .is_ok()
            });
            if sent {
                self.rebuild = RebuildState::Requested;
            } else {
                self.rebuild = RebuildState::Idle;
                window.set_thumbnail_reset_active(false);
                window.set_status_text("Could not rebuild thumbnail cache".into());
            }
        }
        let Some(directory) = &self.directory else {
            return;
        };
        if self.metadata.in_flight.is_none()
            && let Some((index, kind)) = self.next_metadata_job(window)
            && let Some(group) = self.groups.get(index)
        {
            self.metadata
                .submit(directory, group, Some(index), kind, None);
        }
        if self.rebuild != RebuildState::Idle || self.preview.in_flight.is_some() {
            return;
        }
        if let Some((index, kind)) = self.next_visible_job(window)
            && let Some(group) = self.groups.get(index)
        {
            let cache = matches!(kind, JobKind::Thumbnail { .. })
                .then(|| self.thumbnails.cache.clone())
                .flatten();
            self.preview
                .submit(directory, group, Some(index), kind, cache);
            return;
        }
        if window.get_scan_active() || !self.writer.has_room() {
            return;
        }
        let thumbnails = &mut self.thumbnails;
        let Some(cache) = thumbnails.cache.clone() else {
            return;
        };
        let kind = JobKind::Thumbnail {
            scan: thumbnails.scan,
            visible: false,
        };
        if let Some(index) = thumbnails.next_displayed()
            && let Some(group) = self.groups.get(index)
        {
            self.preview
                .submit(directory, group, Some(index), kind, Some(cache));
        } else if let Some(group) = thumbnails.unlisted.front() {
            if self
                .preview
                .submit(directory, group, None, kind, Some(cache))
            {
                thumbnails.unlisted.pop_front();
            }
        } else if thumbnails.unlisted_remaining > 0 && !thumbnails.waiting_for_batch {
            thumbnails.waiting_for_batch = self
                .project_requests
                .try_send(ProjectRequest::ThumbnailBatch(thumbnails.scan))
                .is_ok();
        }
    }

    fn next_visible_job(&self, window: &MainWindow) -> Option<(usize, JobKind)> {
        if window.get_photo_view_active() {
            let preview = window.get_full_preview();
            if preview.state == PreviewState::Pending
                && let Ok(index) = preview.index.try_into()
            {
                return Some((index, JobKind::Preview(PreviewSize::Photo)));
            }
        }
        let active = active_index(window);
        let next = |index: &usize| !self.cached.contains(index) && *index < self.groups.len();
        active
            .filter(|index| self.visible.contains(index) && next(index))
            .or_else(|| self.visible.clone().find(next))
            .or_else(|| active.filter(next))
            .map(|index| {
                (
                    index,
                    JobKind::Thumbnail {
                        scan: self.thumbnails.scan,
                        visible: true,
                    },
                )
            })
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

    fn apply(
        &mut self,
        window: &MainWindow,
        generation: u64,
        index: Option<usize>,
        result: JobResult,
    ) {
        let current = match &result {
            JobResult::Preview(..) | JobResult::Thumbnail(..) => {
                self.preview.in_flight = None;
                &self.preview.generation
            }
            JobResult::Summary(_) | JobResult::Details(_) => {
                self.metadata.in_flight = None;
                &self.metadata.generation
            }
            JobResult::Saved(_) => {
                self.writer.in_flight -= 1;
                &self.writer.generation
            }
        };
        if generation != current.load(Ordering::Relaxed) {
            return;
        }
        let active = active_index(window);
        match (result, index) {
            (JobResult::Preview(size, preview), Some(index)) => {
                self.apply_preview(window, index, size, preview);
            }
            (JobResult::Thumbnail(preview, outcome), index) => {
                if self.thumbnails.needs_thumbnail(index) {
                    let state = match outcome {
                        ThumbnailOutcome::Hit => ThumbnailState::Saved,
                        ThumbnailOutcome::Write(request) => {
                            if self.writer.submit(request, &preview, index) {
                                ThumbnailState::Writing
                            } else {
                                ThumbnailState::Deferred
                            }
                        }
                        ThumbnailOutcome::Unavailable => ThumbnailState::Unavailable,
                    };
                    self.thumbnails.record_load(index, state);
                    self.thumbnails.publish(window);
                }
                if let Some(index) = index {
                    self.apply_preview(window, index, PreviewSize::Thumbnail, preview);
                }
            }
            (JobResult::Saved(saved), index) => {
                self.thumbnails.record_save(index, saved);
                self.thumbnails.publish(window);
            }
            (JobResult::Summary(summary), Some(index)) if active == Some(index) => {
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
            (JobResult::Details(details), Some(index)) if active == Some(index) => {
                window.set_group_details(GroupDetails {
                    index: index as i32,
                    state: PreviewState::Ready,
                    files: metadata_files(details.files),
                    sidecars: metadata_files(details.sidecars),
                });
            }
            _ => (),
        }
    }

    fn apply_preview(
        &mut self,
        window: &MainWindow,
        index: usize,
        size: PreviewSize,
        preview: Preview,
    ) {
        if let PreviewSize::Photo = size {
            if window.get_photo_view_active() && active_index(window) == Some(index) {
                let mut full_preview = FullPreview {
                    index: index as i32,
                    metadata: photo_metadata(preview.filename, preview.summary),
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
            photo.metadata = photo_metadata(preview.filename, preview.summary);
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

fn photo_metadata(filename: String, fields: Fields) -> PhotoMetadata {
    PhotoMetadata {
        filename: filename.into(),
        summary: metadata_model(fields),
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
