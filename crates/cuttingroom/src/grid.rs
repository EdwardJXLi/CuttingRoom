use std::{
    cell::RefCell,
    collections::BTreeSet,
    io,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    thread,
    time::Duration,
};

use async_channel::{Sender, bounded};
use cap_std::fs::Dir;
use slint::{
    ComponentHandle, Image, Model, Rgba8Pixel, SharedPixelBuffer, Timer, VecModel,
    language::StandardListViewItem,
};

use crate::{GridPhoto, MainWindow, PreviewState, thumbnail};

const MAX_VISIBLE_PREVIEWS: usize = 96;
const CACHE_MARGIN: usize = 12;
const UPDATE_INTERVAL: Duration = Duration::from_millis(16);

pub(crate) struct GridSession {
    pub(crate) grid: Rc<RefCell<Grid>>,
    task: slint::JoinHandle<()>,
    worker: thread::JoinHandle<()>,
}

pub(crate) struct Grid {
    directory: Option<Arc<Dir>>,
    paths: Vec<PathBuf>,
    photos: Rc<VecModel<GridPhoto>>,
    visible: Range<usize>,
    cached: BTreeSet<usize>,
    generation: u64,
    busy: bool,
    jobs: Sender<Job>,
}

struct Job {
    directory: Arc<Dir>,
    path: PathBuf,
    index: usize,
    generation: u64,
}

impl GridSession {
    pub(crate) fn start(window: &MainWindow) -> Result<Self, Box<dyn std::error::Error>> {
        let (jobs, requests) = bounded::<Job>(1);
        let (results, completed) = bounded(1);
        let photos = Rc::new(VecModel::default());
        window.set_grid_photos(photos.clone().into());
        let grid = Rc::new(RefCell::new(Grid {
            directory: None,
            paths: Vec::new(),
            photos,
            visible: 0..0,
            cached: BTreeSet::new(),
            generation: 0,
            busy: false,
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
        let task = slint::spawn_local(async move {
            while let Ok((generation, index, thumbnail)) = completed.recv().await {
                let Some(window) = weak.upgrade() else { break };
                state
                    .borrow_mut()
                    .apply(&window, generation, index, thumbnail);

                let state = state.clone();
                let weak = weak.clone();
                Timer::single_shot(UPDATE_INTERVAL, move || {
                    if let Some(window) = weak.upgrade() {
                        let mut grid = state.borrow_mut();
                        grid.busy = false;
                        grid.request_next(&window);
                    }
                });
            }
        })?;

        let worker = thread::Builder::new()
            .name("thumbnail worker".into())
            .spawn(move || {
                while let Ok(job) = requests.recv_blocking() {
                    let result = thumbnail::load(&job.directory, &job.path);
                    if results
                        .send_blocking((job.generation, job.index, result))
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self { grid, task, worker })
    }

    pub(crate) fn finish(self) -> io::Result<()> {
        self.grid.borrow().jobs.close();
        self.task.abort();
        self.worker
            .join()
            .map_err(|_| io::Error::other("thumbnail worker failed"))
    }
}

impl Grid {
    pub(crate) fn replace(
        &mut self,
        window: &MainWindow,
        directory: Option<Dir>,
        paths: Vec<PathBuf>,
    ) {
        let active = active_index(window)
            .and_then(|index| self.paths.get(index))
            .and_then(|path| paths.iter().position(|candidate| candidate == path));

        let file_rows: Vec<_> = paths
            .iter()
            .map(|path| StandardListViewItem::from(path.to_string_lossy().as_ref()))
            .collect();
        let photo_rows: Vec<_> = paths
            .iter()
            .map(|path| grid_photo(path, directory.is_some()))
            .collect();
        self.directory = directory.map(Arc::new);
        self.paths = paths;
        self.generation += 1;
        self.cached.clear();
        self.photos.set_vec(photo_rows);
        window.set_photo_files(Rc::new(VecModel::from(file_rows)).into());
        window.set_active_index(
            active.map_or(if self.paths.is_empty() { -1 } else { 0 }, |index| {
                index as i32
            }),
        );
        self.request_next(window);
    }

    fn refresh(&mut self, window: &MainWindow) {
        let retained = self.retained_range();
        let active = active_index(window);
        self.cached.retain(|index| {
            if retained.contains(index) || active == Some(*index) {
                return true;
            }
            if let Some(mut photo) = self.photos.row_data(*index) {
                photo.preview = Image::default();
                photo.state = PreviewState::Pending;
                self.photos.set_row_data(*index, photo);
            }
            false
        });
        self.request_next(window);
    }

    fn request_next(&mut self, window: &MainWindow) {
        if self.busy {
            return;
        }
        let Some(directory) = &self.directory else {
            return;
        };
        let next = |index: &usize| !self.cached.contains(index) && *index < self.paths.len();
        let index = active_index(window)
            .filter(|index| self.visible.contains(index) && next(index))
            .or_else(|| self.visible.clone().find(next))
            .or_else(|| active_index(window).filter(next));
        if let Some(index) = index
            && let Some(path) = self.paths.get(index)
        {
            self.busy = self
                .jobs
                .try_send(Job {
                    directory: directory.clone(),
                    path: path.clone(),
                    index,
                    generation: self.generation,
                })
                .is_ok();
        }
    }

    fn apply(
        &mut self,
        window: &MainWindow,
        generation: u64,
        index: usize,
        thumbnail: Option<SharedPixelBuffer<Rgba8Pixel>>,
    ) {
        if generation != self.generation
            || !(self.retained_range().contains(&index) || active_index(window) == Some(index))
        {
            return;
        }
        if let Some(mut photo) = self.photos.row_data(index) {
            if let Some(pixels) = thumbnail {
                photo.preview = Image::from_rgba8(pixels);
                photo.state = PreviewState::Ready;
            } else {
                photo.state = PreviewState::Unavailable;
            }
            self.cached.insert(index);
            self.photos.set_row_data(index, photo);
        }
    }

    fn retained_range(&self) -> Range<usize> {
        self.visible.start.saturating_sub(CACHE_MARGIN)
            ..self.visible.end.saturating_add(CACHE_MARGIN)
    }
}

fn active_index(window: &MainWindow) -> Option<usize> {
    window.get_active_index().try_into().ok()
}

fn grid_photo(path: &Path, directory_open: bool) -> GridPhoto {
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let extension = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_uppercase();
    let format = match extension.as_str() {
        "JPG" => "JPEG",
        "TIF" => "TIFF",
        format => format,
    };
    GridPhoto {
        name: name.as_ref().into(),
        relative_path: path.to_string_lossy().as_ref().into(),
        format: format.into(),
        state: if directory_open {
            PreviewState::Pending
        } else {
            PreviewState::Unavailable
        },
        ..GridPhoto::default()
    }
}
