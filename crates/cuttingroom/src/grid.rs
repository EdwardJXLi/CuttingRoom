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
    FullPreview, GridPhoto, GroupDetails, MainWindow, MetadataEntry, MetadataFile, PhotoMetadata,
    PreviewState,
    image_decode::{DecodeError, Decoder},
    metadata::{Fields, GroupMetadata, MetadataSource, read_group},
    preview::{self, Preview, PreviewSize},
};

const MAX_VISIBLE_PREVIEWS: usize = 192;
const CACHE_MARGIN: usize = 12;
const UPDATE_INTERVAL: Duration = Duration::from_millis(16);

pub(crate) struct GridSession {
    pub(crate) grid: Rc<RefCell<Grid>>,
    completed: Receiver<(u64, usize, JobResult)>,
    task: slint::JoinHandle<()>,
    worker: thread::JoinHandle<()>,
}

pub(crate) struct Grid {
    directory: Option<Arc<Dir>>,
    groups: Vec<PhotographGroup>,
    photos: Rc<VecModel<GridPhoto>>,
    visible: Range<usize>,
    cached: BTreeSet<usize>,
    generation: Arc<AtomicU64>,
    in_flight: Option<JobKind>,
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
    Details,
}

enum JobResult {
    Preview(PreviewSize, Preview),
    Details(GroupMetadata),
}

impl GridSession {
    pub(crate) fn start(window: &MainWindow) -> Result<Self, Box<dyn std::error::Error>> {
        let (jobs, requests) = bounded::<Job>(1);
        let (results, completed) = bounded(1);
        let photos = Rc::new(VecModel::default());
        window.set_grid_photos(photos.clone().into());
        let generation = Arc::new(AtomicU64::new(0));
        let grid = Rc::new(RefCell::new(Grid {
            directory: None,
            groups: Vec::new(),
            photos,
            visible: 0..0,
            cached: BTreeSet::new(),
            generation: generation.clone(),
            in_flight: None,
            jobs,
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
                state.borrow_mut().apply(&window, generation, index, result);

                let state = state.clone();
                let weak = weak.clone();
                Timer::single_shot(UPDATE_INTERVAL, move || {
                    if let Some(window) = weak.upgrade() {
                        let mut grid = state.borrow_mut();
                        grid.in_flight = None;
                        grid.request_next(&window);
                    }
                });
            }
        })?;

        let worker = thread::Builder::new()
            .name("preview worker".into())
            .spawn(move || {
                let decoder = Decoder::new();
                while let Ok(job) = requests.recv_blocking() {
                    let cancelled = || {
                        requests.is_closed() || generation.load(Ordering::Relaxed) != job.generation
                    };
                    let result = match job.kind {
                        JobKind::Preview(size) => JobResult::Preview(
                            size,
                            preview::load(&decoder, &job.directory, &job.paths, size, cancelled),
                        ),
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
        Ok(Self {
            grid,
            completed,
            task,
            worker,
        })
    }

    pub(crate) fn finish(self) -> io::Result<()> {
        self.grid.borrow().jobs.close();
        self.completed.close();
        self.task.abort();
        self.worker
            .join()
            .map_err(|_| io::Error::other("preview worker failed"))
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
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.cached.clear();
        self.photos.set_vec(photo_rows);
        window.set_photo_files(Rc::new(VecModel::from(file_rows)).into());
        window.set_active_index(
            active.map_or(if self.groups.is_empty() { -1 } else { 0 }, |index| {
                index as i32
            }),
        );
        self.reset_preview(window, self.preview_target(window));
        self.reset_details(window);
        self.request_next(window);
    }

    fn refresh(&mut self, window: &MainWindow) {
        let target = self.preview_target(window);
        let preview_changed = target != window.get_full_preview().index;
        let details_changed = window.get_active_index() != window.get_group_details().index;
        if preview_changed || (details_changed && matches!(self.in_flight, Some(JobKind::Details)))
        {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        if preview_changed {
            self.reset_preview(window, target);
        }
        if details_changed {
            self.reset_details(window);
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

    fn reset_details(&self, window: &MainWindow) {
        window.set_group_details(GroupDetails {
            index: window.get_active_index(),
            state: if self.directory.is_some() {
                PreviewState::Pending
            } else {
                PreviewState::Unavailable
            },
            ..GroupDetails::default()
        });
    }

    fn request_next(&mut self, window: &MainWindow) {
        if self.in_flight.is_some() {
            return;
        }
        let Some(directory) = &self.directory else {
            return;
        };
        if let Some((index, kind)) = self.next_job(window)
            && let Some(group) = self.groups.get(index)
            && self
                .jobs
                .try_send(Job {
                    directory: directory.clone(),
                    paths: group.files.clone(),
                    index,
                    generation: self.generation.load(Ordering::Relaxed),
                    kind,
                })
                .is_ok()
        {
            self.in_flight = Some(kind);
        }
    }

    fn next_job(&self, window: &MainWindow) -> Option<(usize, JobKind)> {
        if window.get_photo_view_active() {
            let preview = window.get_full_preview();
            if preview.state == PreviewState::Pending
                && let Ok(index) = preview.index.try_into()
            {
                return Some((index, JobKind::Preview(PreviewSize::Photo)));
            }
        }
        let active = active_index(window);
        if window.get_metadata_details_visible()
            && window.get_group_details().state == PreviewState::Pending
            && let Some(index) = active
        {
            return Some((index, JobKind::Details));
        }
        if window.get_photo_view_active() {
            return None;
        }
        let next = |index: &usize| !self.cached.contains(index) && *index < self.groups.len();
        active
            .filter(|index| self.visible.contains(index) && next(index))
            .or_else(|| self.visible.clone().find(next))
            .or_else(|| active.filter(next))
            .map(|index| (index, JobKind::Preview(PreviewSize::Thumbnail)))
    }

    fn apply(&mut self, window: &MainWindow, generation: u64, index: usize, result: JobResult) {
        if generation != self.generation.load(Ordering::Relaxed) {
            return;
        }
        match result {
            JobResult::Preview(size, preview) => self.apply_preview(window, index, size, preview),
            JobResult::Details(details) if active_index(window) == Some(index) => {
                window.set_group_details(GroupDetails {
                    index: index as i32,
                    state: PreviewState::Ready,
                    files: metadata_files(details.files),
                    sidecars: metadata_files(details.sidecars),
                });
            }
            JobResult::Details(_) => (),
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
